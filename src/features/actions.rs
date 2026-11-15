//! Code actions and source edits.
//!
//! The server dispatches `textDocument/codeAction` here.  Actions are built
//! from the analysis' diagnostics (which already carry structured fixes) plus a
//! small set of source-level actions that do not need compiler involvement:
//! import fixes and organization, missing annotations, and package inspection.
//!
//! Formatting helpers live here as well because range formatting is a source
//! edit with the same "never touch text outside the requested range" rule.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, Command, Diagnostic,
    DiagnosticSeverity as LspDiagnosticSeverity, NumberOrString, Position, Range, TextEdit, Url,
    WorkspaceEdit,
};

use crate::analysis::{
    AnalysisDiagnostic, AnalyzedFile, DefinitionKey, DiagnosticSeverity, ModuleSummary,
    ResolvedImport, Signature, SuggestedEdit, SymbolInfo,
};
use crate::syntax::ast::{Definition, Import, ImportPath, Module};
use crate::text::{SourceFile, TextRange};

use super::FeatureContext;

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub fn code_actions(
    ctx: &FeatureContext<'_>,
    params: &CodeActionParams,
) -> Vec<CodeActionOrCommand> {
    let source = &ctx.analysis.source;
    let request = source.text_range(params.range);
    let only = params.context.only.as_deref();
    let mut actions = Vec::new();
    let mut seen = HashSet::new();

    // Structured fixes attached by analysis.  These are the most reliable
    // actions, so they come first; clients usually mark the first quick fix for
    // an error as preferred.
    if wants(only, &CodeActionKind::QUICKFIX) {
        for diagnostic in &ctx.analysis.diagnostics {
            if !ranges_intersect(diagnostic.range, request) {
                continue;
            }
            for (index, fix) in diagnostic.fixes.iter().enumerate() {
                let Some(edit) = workspace_edit_for_fix(ctx, &fix.edits) else {
                    continue;
                };
                let preferred = index == 0;
                push_action(
                    &mut actions,
                    &mut seen,
                    action_with_diagnostic(
                        fix.title.clone(),
                        CodeActionKind::QUICKFIX,
                        Some(diagnostic),
                        Some(&edit),
                        preferred,
                        source,
                        diagnostic.range,
                    ),
                );
            }
        }
    }

    // Unresolved names and modules are handled separately from their
    // diagnostics because an editor may request actions before a diagnostic was
    // published (or with a stale diagnostic set).
    if wants(only, &CodeActionKind::QUICKFIX) {
        let cursor_word = word_at(ctx, request.start);
        let unresolved = ctx
            .analysis
            .reference_at(request.start)
            .is_some_and(|reference| reference.key.is_none());

        for diagnostic in &ctx.analysis.diagnostics {
            if !ranges_intersect(diagnostic.range, request) {
                continue;
            }
            let code = diagnostic.code.to_ascii_lowercase();
            let message = diagnostic.message.to_ascii_lowercase();
            let unknown_module = code.contains("module")
                && (code.contains("unknown") || code.contains("unresolved"))
                || message.contains("module")
                    && (message.contains("could not be found")
                        || message.contains("unknown")
                        || message.contains("not found"));
            let unknown_name = code.contains("unknown-name")
                || code.contains("exposing")
                || message.contains("not defined")
                || message.contains("unknown name")
                || message.contains("cannot find")
                || message.contains("not exposed")
                || message.contains("exposing");

            if unknown_module {
                if let Some(name) = diagnostic_name(source, diagnostic) {
                    let name = name
                        .split(['.', '/'])
                        .next()
                        .unwrap_or(name.as_str())
                        .to_string();
                    for suggestion in module_import_suggestions(ctx, &name) {
                        if let Some(edit) = import_edit(ctx, &suggestion.path, None::<&str>) {
                            push_action(
                                &mut actions,
                                &mut seen,
                                action_with_diagnostic(
                                    format!("Import `{}`", suggestion.path),
                                    CodeActionKind::QUICKFIX,
                                    Some(diagnostic),
                                    Some(&edit),
                                    suggestion.preferred,
                                    source,
                                    diagnostic.range,
                                ),
                            );
                        }
                    }
                }
            }

            if unknown_name {
                if let Some(name) = diagnostic_name(source, diagnostic) {
                    if let Some((module, _)) = name.split_once('.') {
                        for suggestion in module_import_suggestions(ctx, module) {
                            if let Some(edit) = import_edit(ctx, &suggestion.path, None::<&str>) {
                                push_action(
                                    &mut actions,
                                    &mut seen,
                                    action_with_diagnostic(
                                        format!("Import `{}`", suggestion.path),
                                        CodeActionKind::QUICKFIX,
                                        Some(diagnostic),
                                        Some(&edit),
                                        suggestion.preferred,
                                        source,
                                        diagnostic.range,
                                    ),
                                );
                            }
                        }
                    }
                    let mut suggestions =
                        unknown_name_actions(ctx, &name, diagnostic.range, Some(diagnostic));
                    for action in suggestions.drain(..) {
                        push_action(&mut actions, &mut seen, action);
                    }
                }
            }
        }

        // A cursor on an unresolved identifier is a useful fallback when no
        // diagnostic arrived yet.  Only act when analysis actually marked the
        // reference unresolved; this keeps suggestions out of comments and
        // string literals.
        if unresolved {
            if let Some((name, range)) = cursor_word {
                let mut suggestions = unknown_name_actions(ctx, &name, range, None);
                for action in suggestions.drain(..) {
                    push_action(&mut actions, &mut seen, action);
                }
            }
        }
    }

    // Source actions are document-wide by definition; the request range only
    // decides whether the client asked for them via `context.only`.
    let organize = if wants(only, &CodeActionKind::SOURCE_ORGANIZE_IMPORTS)
        || wants(only, &CodeActionKind::SOURCE_FIX_ALL)
        || only.is_none()
    {
        organize_imports_edit(ctx)
    } else {
        None
    };
    if let Some(edit) = organize {
        let kind = if only.is_none() || wants(only, &CodeActionKind::SOURCE_ORGANIZE_IMPORTS) {
            CodeActionKind::SOURCE_ORGANIZE_IMPORTS
        } else {
            CodeActionKind::SOURCE_FIX_ALL
        };
        push_action(
            &mut actions,
            &mut seen,
            action_with_diagnostic(
                "Organize imports".to_string(),
                kind,
                None,
                Some(&edit),
                true,
                source,
                request,
            ),
        );
    }

    if wants(only, &CodeActionKind::QUICKFIX) {
        for action in annotation_actions(ctx, request) {
            push_action(&mut actions, &mut seen, action);
        }
    }

    if only.is_none() || wants(only, &CodeActionKind::QUICKFIX) {
        actions.extend(package_actions(ctx, request));
    }

    actions
}

/// Fill in an edit for a deferred action.  All actions produced by
/// [`code_actions`] are normally complete; the `data` fallback exists so future
/// (or lazily computed) actions can be resolved without server changes.
pub fn resolve_code_action(ctx: &FeatureContext<'_>, mut action: CodeAction) -> CodeAction {
    if action.edit.is_some() {
        return action;
    }
    let Some(data) = action.data.clone() else {
        return action;
    };
    let Some(kind) = data.get("kind").and_then(|value| value.as_str()) else {
        return action;
    };
    match kind {
        "import" => {
            if let Some(path) = data.get("path").and_then(|value| value.as_str()) {
                let exposing = data.get("exposing").and_then(|value| value.as_str());
                if let Some(edit) = import_edit(ctx, path, exposing) {
                    action.edit = Some(edit);
                }
            }
        }
        "organize-imports" => {
            if let Some(edit) = organize_imports_edit(ctx) {
                action.edit = Some(edit);
            }
        }
        "annotation" => {
            if let (Some(name), Some(ty)) = (
                data.get("name").and_then(|value| value.as_str()),
                data.get("type").and_then(|value| value.as_str()),
            ) {
                if let Some(edit) = annotation_edit(ctx, name, ty) {
                    action.edit = Some(edit);
                }
            }
        }
        _ => {}
    }
    action
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

/// Compute minimal line-oriented edits that turn `source` into `formatted`
/// inside `range`.
///
/// Both sides of a changed hunk are expanded to full source lines, so applying
/// the edits never splits a line.  Hunks that extend beyond `range` are skipped
/// rather than clamped: a range-format request must not rewrite unrelated code.
pub fn line_diff_edits(source: &str, formatted: &str, range: Range) -> Option<Vec<TextEdit>> {
    if source == formatted {
        return None;
    }

    let index = LineIndex::new(source);
    let source_lines: Vec<&str> = source.split('\n').collect();
    let formatted_lines: Vec<&str> = formatted.split('\n').collect();

    if source_lines.len() != formatted_lines.len() {
        // Line-oriented hunk diffing cannot prove that a subset of a line-count
        // changing edit is self-contained: `roc fmt` may move a definition to a
        // different line. Only answer when the request covers the whole file,
        // where a single full replacement is exact.
        let end = index.position(source.len() as u32);
        let covers_document = range.start == Position::new(0, 0)
            && (range.end >= end || range.end.line as usize >= index.starts.len());
        return if covers_document {
            full_document_edit(source, formatted).map(|edit| vec![edit])
        } else {
            None
        };
    }

    let start_line = (range.start.line as usize).min(index.starts.len());
    let end_line = (range.end.line as usize).min(index.starts.len());
    let allowed_start = start_line;
    let allowed_end = if range.end.line as usize >= index.starts.len() {
        index.starts.len()
    } else if range.end.character == 0 && range.end.line != range.start.line {
        end_line
    } else {
        (end_line + 1).min(index.starts.len())
    }
    .max(allowed_start);

    let mut edits = Vec::new();
    for (line, (old, new)) in source_lines.iter().zip(formatted_lines.iter()).enumerate() {
        if old == new || line < allowed_start || line + 1 > allowed_end {
            continue;
        }
        let start = index.offset(line);
        let end = index.line_end(line);
        edits.push(TextEdit {
            range: Range {
                start: index.position(start),
                end: index.position(end),
            },
            new_text: new.to_string(),
        });
    }

    if edits.is_empty() {
        None
    } else {
        Some(edits)
    }
}

/// The safe fallback when a formatter can only produce a whole document.
pub fn full_document_edit(source: &str, formatted: &str) -> Option<TextEdit> {
    if source == formatted {
        return None;
    }
    let index = LineIndex::new(source);
    let end = index.position(source.len() as u32);
    Some(TextEdit {
        range: Range {
            start: Position::new(0, 0),
            end,
        },
        new_text: formatted.to_string(),
    })
}

#[derive(Debug)]
struct LineIndex<'a> {
    text: &'a str,
    starts: Vec<u32>,
    len: u32,
}

impl<'a> LineIndex<'a> {
    fn new(text: &'a str) -> Self {
        let mut starts = vec![0u32];
        for (offset, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(offset as u32 + 1);
            }
        }
        Self {
            text,
            starts,
            len: text.len() as u32,
        }
    }

    fn offset(&self, line: usize) -> u32 {
        self.starts.get(line).copied().unwrap_or(self.len)
    }

    fn line_end(&self, line: usize) -> u32 {
        let start = self.offset(line);
        let mut end = match self.starts.get(line + 1) {
            Some(next) => next.saturating_sub(1),
            None => self.len,
        };
        if end > start && self.text.as_bytes().get(end as usize - 1) == Some(&b'\r') {
            end -= 1;
        }
        end
    }

    fn position(&self, offset: u32) -> Position {
        let offset = offset.min(self.len);
        let line = match self.starts.binary_search(&offset) {
            Ok(line) => line,
            Err(next) => next.saturating_sub(1),
        };
        let start = self.offset(line) as usize;
        let end = (offset as usize).max(start).min(self.text.len());
        let character = self.text[start..end]
            .chars()
            .map(|ch| ch.len_utf16() as u32)
            .sum();
        Position::new(line as u32, character)
    }
}

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

fn import_edit(
    ctx: &FeatureContext<'_>,
    path: &str,
    exposing: Option<&str>,
) -> Option<WorkspaceEdit> {
    let source = &ctx.analysis.source;
    let (range, text) = import_insertion(source, &ctx.analysis.parsed.module, path, exposing)?;
    Some(single_file_edit(source, &ctx.analysis.uri, range, text))
}

fn import_insertion(
    source: &SourceFile,
    module: &Module,
    path: &str,
    exposing: Option<&str>,
) -> Option<(TextRange, String)> {
    let statement = import_statement(path, exposing);
    let text = source.text();
    let imports = &module.imports;

    if let Some(next) = imports
        .iter()
        .filter(|import| import.path.raw() > path)
        .min_by_key(|import| import.range.start)
    {
        let mut line = source.line_of_offset(next.range.start);
        while line > 0 && source.line_text(line - 1).trim_start().starts_with('#') {
            line -= 1;
        }
        let offset = source.line_start(line);
        return Some((TextRange::empty(offset), format!("{statement}\n")));
    }

    if let Some(last) = imports.iter().max_by_key(|import| import.range.end) {
        let line = source.line_of_offset(last.range.end.saturating_sub(1));
        let offset = after_line(source, line);
        let had_newline = offset > source.line_end(line);
        let insertion = if had_newline || offset >= source.len() {
            format!("{statement}\n")
        } else {
            format!("\n{statement}")
        };
        return Some((TextRange::empty(offset), insertion));
    }

    if let Some(header) = module.header.as_ref() {
        let line = source.line_of_offset(header.range.end.saturating_sub(1));
        let offset = after_line(source, line);
        let rest = text.get(offset as usize..).unwrap_or("");
        let insertion = if rest.is_empty() {
            format!("{statement}\n")
        } else {
            format!("{statement}\n\n")
        };
        return Some((TextRange::empty(offset), insertion));
    }

    // No header and no imports: imports go at the very top of the file.
    let insertion = if text.is_empty() {
        format!("{statement}\n")
    } else {
        format!("{statement}\n\n")
    };
    Some((TextRange::empty(0), insertion))
}

fn import_statement(path: &str, exposing: Option<&str>) -> String {
    match exposing {
        Some(name) if !name.is_empty() => format!("import {path} exposing [{name}]"),
        _ => format!("import {path}"),
    }
}

fn after_line(source: &SourceFile, line: usize) -> u32 {
    let mut offset = source.line_end(line);
    let bytes = source.text().as_bytes();
    while offset < source.len()
        && bytes
            .get(offset as usize)
            .is_some_and(|byte| *byte == b'\r' || *byte == b'\n')
    {
        offset += 1;
    }
    offset
}

fn default_qualifier(import: &Import) -> String {
    if let Some(alias) = &import.alias {
        return alias.text.clone();
    }
    match &import.path {
        ImportPath::Roc { raw, .. } => raw.rsplit(['.', '/']).next().unwrap_or(raw).to_string(),
        ImportPath::File { .. } => String::new(),
    }
}

fn effective_qualifier(qualifier: &str, import: &Import) -> String {
    let last = qualifier.rsplit(['.', '/']).next().unwrap_or(qualifier);
    if last.is_empty() {
        default_qualifier(import)
    } else {
        last.to_string()
    }
}

fn import_path_matches(import: &Import, module: &ModuleSummary) -> bool {
    match &import.path {
        ImportPath::Roc { raw, .. } => {
            raw == &module.name
                || module.name.rsplit('/').next() == Some(raw.as_str())
                || raw.rsplit('.').next() == module.name.rsplit('/').next()
        }
        ImportPath::File { .. } => false,
    }
}

fn module_import_path(module: &ModuleSummary) -> String {
    match module.package.as_deref() {
        Some(package) if !package.is_empty() => {
            if module.name.starts_with(&format!("{package}.")) {
                module.name.clone()
            } else {
                format!("{package}.{}", module.name)
            }
        }
        _ => module.name.clone(),
    }
}

fn module_qualifier(module: &ModuleSummary) -> String {
    module
        .name
        .rsplit(['.', '/'])
        .next()
        .unwrap_or(module.name.as_str())
        .to_string()
}

struct ImportSuggestion {
    path: String,
    preferred: bool,
}

fn module_import_suggestions(ctx: &FeatureContext<'_>, name: &str) -> Vec<ImportSuggestion> {
    let workspace = ctx.host.workspace();
    let mut suggestions = Vec::new();
    let mut seen = HashSet::new();

    for module in &workspace.modules {
        let last = module
            .name
            .rsplit('/')
            .next()
            .unwrap_or(module.name.as_str());
        if module.name == name || last == name {
            let path = module_import_path(module);
            if seen.insert(path.clone()) {
                suggestions.push(ImportSuggestion {
                    path,
                    preferred: true,
                });
            }
        }
    }

    for package in &workspace.packages {
        if package.shorthand != name {
            continue;
        }
        let mut modules: Vec<String> = package
            .modules
            .iter()
            .chain(package.exposes.iter())
            .cloned()
            .collect();
        modules.sort();
        modules.dedup();
        for module in modules.into_iter().take(5) {
            let path = format!("{}.{}", package.shorthand, module);
            if seen.insert(path.clone()) {
                suggestions.push(ImportSuggestion {
                    path,
                    preferred: true,
                });
            }
        }
    }

    suggestions
}

fn organize_imports_edit(ctx: &FeatureContext<'_>) -> Option<WorkspaceEdit> {
    let source = &ctx.analysis.source;
    let module = &ctx.analysis.parsed.module;
    if module.imports.is_empty() {
        return None;
    }

    let (block, mut items) = import_block(ctx)?;
    if block.is_empty() {
        return None;
    }

    let old_block = source.slice(block).to_string();
    items.sort_by(|left, right| {
        left.key
            .cmp(&right.key)
            .then_with(|| left.start_line.cmp(&right.start_line))
    });

    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.text.trim_end().to_string()));

    let resolved: HashMap<u32, &ResolvedImport> = ctx
        .analysis
        .imports
        .iter()
        .map(|import| (import.import.range.start, import))
        .collect();

    let mut kept = Vec::new();
    for item in items {
        let import = &module.imports[item.import_index];
        if import_is_file(import)
            || import_used(
                ctx,
                import,
                resolved.get(&import.range.start).copied(),
                block,
            )
        {
            kept.push(item);
        }
    }

    let mut replacement = kept
        .iter()
        .map(|item| item.text.trim_end_matches('\n'))
        .collect::<Vec<_>>()
        .join("\n");
    if old_block.ends_with('\n') && !replacement.is_empty() {
        replacement.push('\n');
    }

    if replacement == old_block {
        return None;
    }

    Some(single_file_edit(
        source,
        &ctx.analysis.uri,
        block,
        replacement,
    ))
}

struct ImportItem {
    start_line: usize,
    key: String,
    text: String,
    import_index: usize,
}

fn import_block(ctx: &FeatureContext<'_>) -> Option<(TextRange, Vec<ImportItem>)> {
    let source = &ctx.analysis.source;
    let imports = &ctx.analysis.parsed.module.imports;
    if imports.is_empty() {
        return None;
    }

    let mut starts: HashMap<usize, usize> = HashMap::new();
    let mut ends: HashMap<usize, usize> = HashMap::new();
    let mut attached: HashMap<usize, usize> = HashMap::new();
    for (index, import) in imports.iter().enumerate() {
        let start = source.line_of_offset(import.range.start);
        let end = source.line_of_offset(import.range.end.saturating_sub(1)) + 1;
        if starts.insert(start, index).is_some() || end <= start {
            return None;
        }
        ends.insert(start, end);
        let mut attached_line = start;
        while attached_line > 0 {
            let previous = source.line_text(attached_line - 1);
            if previous.trim_start().starts_with('#') {
                attached_line -= 1;
            } else {
                break;
            }
        }
        attached.insert(start, attached_line);
    }

    let block_start_line = attached.values().copied().min()?;
    let block_end_line = ends.values().copied().max()?;
    let mut items = Vec::new();
    let mut pending_comment: Option<usize> = None;
    let mut line = block_start_line;

    while line < block_end_line {
        if let Some(index) = starts.get(&line).copied() {
            let end = ends.get(&line).copied()?;
            let start = pending_comment.take().unwrap_or_else(|| attached[&line]);
            let import = &imports[index];
            items.push(ImportItem {
                start_line: start,
                key: import.path.raw().to_string(),
                text: lines_between(source, start, end),
                import_index: index,
            });
            line = end;
            continue;
        }

        let text = source.line_text(line);
        if text.trim().is_empty() {
            if pending_comment.is_some() {
                // A comment separated from any import cannot be moved with an
                // import; leave the file alone rather than deleting it.
                return None;
            }
        } else if text.trim_start().starts_with('#') {
            pending_comment.get_or_insert(line);
        } else {
            return None;
        }
        line += 1;
    }

    if pending_comment.is_some() {
        return None;
    }

    let start = source.line_start(block_start_line);
    let end = source.line_start(block_end_line);
    Some((TextRange::new(start, end), items))
}

fn lines_between(source: &SourceFile, start: usize, end: usize) -> String {
    let mut output = String::new();
    for line in start..end {
        if line > start {
            output.push('\n');
        }
        output.push_str(source.line_text(line));
    }
    output
}

fn import_is_file(import: &Import) -> bool {
    matches!(&import.path, ImportPath::File { .. })
}

fn import_used(
    ctx: &FeatureContext<'_>,
    import: &Import,
    resolved: Option<&ResolvedImport>,
    block: TextRange,
) -> bool {
    if import_is_file(import) {
        return true;
    }
    if ctx.analysis.references.is_empty() && ctx.analysis.parsed.tokens.is_empty() {
        // Without either a token stream or reference table there is no evidence
        // that the import is unused; deleting it would be a guess.
        return true;
    }

    let qualifier = resolved
        .map(|resolved| effective_qualifier(&resolved.qualifier, import))
        .unwrap_or_else(|| default_qualifier(import));
    if qualifier.is_empty() {
        return true;
    }

    let mut names: Vec<String> = resolved
        .map(|resolved| resolved.exposing.clone())
        .unwrap_or_default();
    if names.is_empty() {
        names = import
            .exposing
            .iter()
            .map(|name| name.text.clone())
            .collect();
    }

    let source = &ctx.analysis.source;
    let text = source.text();
    let matches_name = |candidate: &str| {
        candidate == qualifier
            || candidate
                .strip_prefix(&qualifier)
                .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('/'))
            || names.iter().any(|name| name == candidate)
    };

    for reference in &ctx.analysis.references {
        if block.contains_inclusive(reference.range.start) {
            continue;
        }
        if matches_name(&reference.text) {
            return true;
        }
    }

    // Lexical fallback: when analysis is incomplete (a file is still being
    // edited), token scanning is conservative and prevents deleting imports
    // that are visibly used.
    for token in &ctx.analysis.parsed.tokens {
        if token.kind.is_trivia() {
            continue;
        }
        if block.contains_inclusive(token.range.start) {
            continue;
        }
        if matches_name(token.text(text)) {
            return true;
        }
    }

    // An imported module that resolves to this workspace but whose analysis
    // failed still gets the benefit of the doubt.
    false
}

// ---------------------------------------------------------------------------
// Unknown names and modules
// ---------------------------------------------------------------------------

fn unknown_name_actions(
    ctx: &FeatureContext<'_>,
    name: &str,
    range: TextRange,
    diagnostic: Option<&AnalysisDiagnostic>,
) -> Vec<CodeActionOrCommand> {
    let source = &ctx.analysis.source;
    let mut actions = Vec::new();
    let workspace = ctx.host.workspace();

    for suggestion in module_import_suggestions(ctx, name) {
        if let Some(edit) = import_edit(ctx, &suggestion.path, None::<&str>) {
            actions.push(action_with_diagnostic(
                format!("Import `{}`", suggestion.path),
                CodeActionKind::QUICKFIX,
                diagnostic,
                Some(&edit),
                suggestion.preferred,
                source,
                range,
            ));
        }
    }

    for resolved in &ctx.analysis.imports {
        if import_is_file(&resolved.import) {
            continue;
        }
        let target = resolved.target.as_ref().or_else(|| {
            workspace
                .modules
                .iter()
                .find(|module| import_path_matches(&resolved.import, module))
        });
        let Some(target) = target else {
            continue;
        };
        if target.export(name).is_none() && !resolved.exposing.iter().any(|n| n == name) {
            continue;
        }

        let qualifier = effective_qualifier(&resolved.qualifier, &resolved.import);
        if qualifier.is_empty() {
            continue;
        }

        if !resolved.exposing.iter().any(|exposed| exposed == name) {
            if let Some(edit) = add_exposing_edit(ctx, &resolved.import, name) {
                actions.push(action_with_diagnostic(
                    format!(
                        "Add `{name}` to `import {}` exposing",
                        resolved.import.path.raw()
                    ),
                    CodeActionKind::QUICKFIX,
                    diagnostic,
                    Some(&edit),
                    true,
                    source,
                    range,
                ));
            }
        }

        if qualifier != name && !range.is_empty() {
            let edit = single_file_edit(
                source,
                &ctx.analysis.uri,
                range,
                format!("{qualifier}.{name}"),
            );
            actions.push(action_with_diagnostic(
                format!("Qualify as `{qualifier}.{name}`"),
                CodeActionKind::QUICKFIX,
                diagnostic,
                Some(&edit),
                false,
                source,
                range,
            ));
        }
    }

    // Builtins are available without imports but are not unqualified.  Offer
    // the canonical module qualifier when it is unambiguous.
    let mut builtin_matches = Vec::new();
    for module in ctx.host.builtins().modules() {
        for item in &module.items {
            if item.short_name == name {
                builtin_matches.push((module.name.as_str(), item.name.as_str()));
            }
        }
    }
    builtin_matches.sort();
    builtin_matches.dedup();
    for (module, full_name) in builtin_matches {
        let qualifier = module.rsplit('.').next().unwrap_or(module);
        if full_name != name && qualifier != name && !range.is_empty() {
            let edit = single_file_edit(
                source,
                &ctx.analysis.uri,
                range,
                format!("{qualifier}.{name}"),
            );
            actions.push(action_with_diagnostic(
                format!("Qualify as `{qualifier}.{name}`"),
                CodeActionKind::QUICKFIX,
                diagnostic,
                Some(&edit),
                false,
                source,
                range,
            ));
        }
    }

    // A top-level item in another workspace module: import it (either exposing
    // the item for unqualified use, or qualifying every use).
    let mut modules: Vec<&ModuleSummary> = workspace
        .modules
        .iter()
        .filter(|module| module.uri != ctx.analysis.uri && module.export(name).is_some())
        .collect();
    modules.sort_by(|left, right| {
        module_import_path(left)
            .cmp(&module_import_path(right))
            .then_with(|| left.uri.cmp(&right.uri))
    });
    modules.dedup_by(|left, right| module_import_path(left) == module_import_path(right));

    for module in modules {
        let path = module_import_path(module);
        let qualifier = module_qualifier(module);

        if let Some((import_range, import_text)) =
            import_insertion(source, &ctx.analysis.parsed.module, &path, Some(name))
        {
            let edit = single_file_edit(source, &ctx.analysis.uri, import_range, import_text);
            actions.push(action_with_diagnostic(
                format!("Import `{path}` exposing [`{name}`]"),
                CodeActionKind::QUICKFIX,
                diagnostic,
                Some(&edit),
                true,
                source,
                range,
            ));
        }

        if qualifier != name && !range.is_empty() {
            if let Some((import_range, import_text)) =
                import_insertion(source, &ctx.analysis.parsed.module, &path, None)
            {
                let mut changes = HashMap::new();
                changes.insert(
                    ctx.analysis.uri.clone(),
                    vec![
                        TextEdit {
                            range: safe_lsp_range(source, import_range),
                            new_text: import_text,
                        },
                        TextEdit {
                            range: safe_lsp_range(source, range),
                            new_text: format!("{qualifier}.{name}"),
                        },
                    ],
                );
                let edit = WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                };
                actions.push(action_with_diagnostic(
                    format!("Import `{path}` and qualify as `{qualifier}.{name}`"),
                    CodeActionKind::QUICKFIX,
                    diagnostic,
                    Some(&edit),
                    false,
                    source,
                    range,
                ));
            }
        }
    }

    actions
}

fn add_exposing_edit(
    ctx: &FeatureContext<'_>,
    import: &Import,
    name: &str,
) -> Option<WorkspaceEdit> {
    if import.exposing.iter().any(|existing| existing.text == name) {
        return None;
    }
    let source = &ctx.analysis.source;
    let text = source.text();
    if let Some(last) = import.exposing.last() {
        let tail = text.get(last.range.end as usize..import.range.end as usize)?;
        let bracket = tail.find(']')?;
        let offset = last.range.end + bracket as u32;
        let before = text.get(last.range.end as usize..offset as usize)?;
        let separator = if before.trim_end().ends_with(',') {
            " "
        } else {
            ", "
        };
        return Some(single_file_edit(
            source,
            &ctx.analysis.uri,
            TextRange::empty(offset),
            format!("{separator}{name}"),
        ));
    }

    Some(single_file_edit(
        source,
        &ctx.analysis.uri,
        TextRange::empty(import.range.end),
        format!(" exposing [{name}]"),
    ))
}

fn diagnostic_name(source: &SourceFile, diagnostic: &AnalysisDiagnostic) -> Option<String> {
    let slice = source.slice(diagnostic.range).trim();
    if is_plain_name(slice) {
        return Some(slice.to_string());
    }
    if let Some(name) = backtick_name(&diagnostic.message) {
        return Some(name);
    }
    crate::analysis::identifier_at(source, diagnostic.range.start).map(|(name, _)| name)
}

fn backtick_name(message: &str) -> Option<String> {
    let mut parts = message.split('`');
    parts.next()?;
    let name = parts.next()?.trim();
    if is_plain_name(name) {
        Some(name.to_string())
    } else {
        None
    }
}

fn is_plain_name(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric() || ch == '_' || ch == '!' || ch == '/' || ch == '.'
        })
}

// ---------------------------------------------------------------------------
// Missing annotations
// ---------------------------------------------------------------------------

fn annotation_actions(ctx: &FeatureContext<'_>, request: TextRange) -> Vec<CodeActionOrCommand> {
    let source = &ctx.analysis.source;
    let mut actions = Vec::new();
    for definition in ctx.analysis.definitions() {
        if !ranges_intersect(definition.range, request) {
            continue;
        }
        if definition.annotation.is_some() {
            continue;
        }
        let Some(name) = definition.simple_name() else {
            continue;
        };
        let Some(annotation) = concrete_annotation(ctx.analysis, definition, &name.text) else {
            continue;
        };
        let line = source.line_of_offset(name.range.start);
        let line_start = source.line_start(line);
        let indent_range = TextRange::new(line_start, name.range.start);
        let indent = source.slice(indent_range);
        let indent = if indent.chars().all(char::is_whitespace) {
            indent
        } else {
            ""
        };
        let edit = single_file_edit(
            source,
            &ctx.analysis.uri,
            TextRange::empty(line_start),
            format!("{indent}{annotation}\n"),
        );
        actions.push(action_with_diagnostic(
            format!("Add type annotation: `{annotation}`"),
            CodeActionKind::QUICKFIX,
            None,
            Some(&edit),
            false,
            source,
            name.range,
        ));
    }
    actions
}

fn annotation_edit(ctx: &FeatureContext<'_>, name: &str, ty: &str) -> Option<WorkspaceEdit> {
    let source = &ctx.analysis.source;
    for definition in ctx.analysis.definitions() {
        let Some(defined) = definition.simple_name() else {
            continue;
        };
        if defined.text != name {
            continue;
        }
        let line = source.line_of_offset(defined.range.start);
        let line_start = source.line_start(line);
        return Some(single_file_edit(
            source,
            &ctx.analysis.uri,
            TextRange::empty(line_start),
            format!("{name} : {ty}\n"),
        ));
    }
    None
}

fn concrete_annotation(
    analysis: &AnalyzedFile,
    definition: &Definition,
    name: &str,
) -> Option<String> {
    if let Some(signature) = signature_for_definition(analysis, definition, name) {
        if let Some(annotation) = annotation_from_signature(signature, name) {
            return Some(annotation);
        }
        if let Some(annotation) = annotation_from_display(&signature.display, name) {
            return Some(annotation);
        }
    }

    let detail = symbol_detail_for_definition(analysis, definition)?;
    let detail = detail.trim();
    let ty = detail
        .split_once(':')
        .map(|(_, rest)| rest.trim())
        .unwrap_or(detail);
    let looks_like_type = detail.contains(':')
        || ty.contains("->")
        || ty.contains("=>")
        || ty
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase() || matches!(ch, '(' | '[' | '{'));
    if !is_concrete_type(ty) || !looks_like_type {
        return None;
    }
    Some(format!("{name} : {ty}"))
}

fn annotation_from_display(display: &str, name: &str) -> Option<String> {
    let display = display.trim();
    if display.is_empty() {
        return None;
    }
    let prefix = format!("{name} :");
    if let Some(rest) = display.strip_prefix(&prefix) {
        return is_concrete_type(rest).then(|| display.to_string());
    }
    if let Some(rest) = display.strip_prefix(&format!("{name}:")) {
        return is_concrete_type(rest).then(|| format!("{name} : {rest}"));
    }
    if !is_concrete_type(display) {
        return None;
    }
    let ty = display
        .split_once(':')
        .map(|(_, rest)| rest.trim())
        .unwrap_or(display);
    let looks_like_type = ty.contains("->")
        || ty.contains("=>")
        || ty
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase() || matches!(ch, '(' | '[' | '{'));
    if !is_concrete_type(ty) || !looks_like_type {
        return None;
    }
    Some(format!("{name} : {ty}"))
}

fn annotation_from_signature(signature: &Signature, name: &str) -> Option<String> {
    let mut params = Vec::with_capacity(signature.params.len());
    for param in &signature.params {
        let ty = param.ty.as_deref()?.trim();
        if !is_concrete_type(ty) {
            return None;
        }
        params.push(ty);
    }
    let return_type = signature.return_type.as_deref()?.trim();
    if !is_concrete_type(return_type) {
        return None;
    }

    let mut annotation = format!("{name} : ");
    if !params.is_empty() {
        annotation.push_str(&params.join(", "));
        annotation.push(' ');
    }
    annotation.push_str(if signature.is_effectful { "=>" } else { "->" });
    annotation.push(' ');
    annotation.push_str(return_type);
    Some(annotation)
}

fn symbol_detail_for_definition<'a>(
    analysis: &'a AnalyzedFile,
    definition: &Definition,
) -> Option<&'a str> {
    let defined = definition.simple_name()?;
    symbol_detail_for_range(&analysis.symbols, defined.range)
}

fn symbol_detail_for_range(symbols: &[SymbolInfo], range: TextRange) -> Option<&str> {
    for symbol in symbols {
        if symbol.name_range == range {
            return symbol.detail.as_deref();
        }
        if let Some(detail) = symbol_detail_for_range(&symbol.children, range) {
            return Some(detail);
        }
    }
    None
}

fn signature_for_definition<'a>(
    analysis: &'a AnalyzedFile,
    definition: &Definition,
    name: &str,
) -> Option<&'a Signature> {
    if let Some(defined) = definition.simple_name() {
        if let Some(key) = symbol_key_for_range(&analysis.symbols, defined.range) {
            if let Some(signature) = analysis.signature_for(key) {
                return Some(signature);
            }
        }
    }
    analysis
        .signatures
        .values()
        .find(|signature| signature.name == name)
}

fn symbol_key_for_range(symbols: &[SymbolInfo], range: TextRange) -> Option<&DefinitionKey> {
    for symbol in symbols {
        if symbol.name_range == range {
            return Some(&symbol.key);
        }
        if let Some(key) = symbol_key_for_range(&symbol.children, range) {
            return Some(key);
        }
    }
    None
}

fn is_concrete_type(ty: &str) -> bool {
    let ty = ty.trim();
    !ty.is_empty()
        && !ty.contains('_')
        && !ty.contains('?')
        && ty != "…"
        && !ty.contains("unknown")
        && !ty.contains("<inferred>")
}

// ---------------------------------------------------------------------------
// Package inspection
// ---------------------------------------------------------------------------

fn package_actions(ctx: &FeatureContext<'_>, request: TextRange) -> Vec<CodeActionOrCommand> {
    let source = &ctx.analysis.source;
    let Some(header) = ctx.analysis.parsed.module.header.as_ref() else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    let header_hit = ranges_intersect(header.range, request)
        || header
            .dependencies
            .iter()
            .any(|dependency| ranges_intersect(dependency.range, request));
    if header_hit {
        actions.push(CodeActionOrCommand::Command(Command {
            title: "Show dependency tree".to_string(),
            command: "hardroc.deps".to_string(),
            arguments: Some(vec![serde_json::Value::String(
                ctx.analysis.uri.to_string(),
            )]),
        }));
    }

    for dependency in &header.dependencies {
        if !ranges_intersect(dependency.range, request) {
            continue;
        }
        let target = match &dependency.source {
            crate::syntax::ast::DependencySource::Path(path) => source
                .path()
                .and_then(|file| file.parent().map(|parent| parent.join(path)))
                .and_then(|path| Url::from_file_path(path).ok()),
            crate::syntax::ast::DependencySource::Url(url) => Url::parse(url).ok(),
            crate::syntax::ast::DependencySource::RocVersion(_)
            | crate::syntax::ast::DependencySource::CompilerOwned(_) => None,
        };
        if let Some(target) = target {
            actions.push(CodeActionOrCommand::Command(Command {
                title: format!("Open package source for `{}`", dependency.shorthand.text),
                command: "hardroc.openPackageSource".to_string(),
                arguments: Some(vec![serde_json::Value::String(target.to_string())]),
            }));
        }
    }
    actions
}

// ---------------------------------------------------------------------------
// Action helpers
// ---------------------------------------------------------------------------

fn wants(only: Option<&[CodeActionKind]>, kind: &CodeActionKind) -> bool {
    let Some(only) = only else {
        return true;
    };
    let actual = kind.as_str();
    only.iter().any(|requested| {
        let requested = requested.as_str();
        actual == requested || actual.starts_with(&format!("{requested}."))
    })
}

fn push_action(
    actions: &mut Vec<CodeActionOrCommand>,
    seen: &mut HashSet<String>,
    action: CodeActionOrCommand,
) {
    let key = match &action {
        CodeActionOrCommand::CodeAction(action) => format!(
            "{}\u{0}{}",
            action.title,
            action
                .kind
                .as_ref()
                .map(CodeActionKind::as_str)
                .unwrap_or("")
        ),
        CodeActionOrCommand::Command(command) => {
            format!("{}\u{0}{}", command.title, command.command)
        }
    };
    if seen.insert(key) {
        actions.push(action);
    }
}

fn action_with_diagnostic(
    title: String,
    kind: CodeActionKind,
    diagnostic: Option<&AnalysisDiagnostic>,
    edit: Option<&WorkspaceEdit>,
    preferred: bool,
    source: &SourceFile,
    data_range: TextRange,
) -> CodeActionOrCommand {
    let data = if edit.is_none() {
        Some(serde_json::json!({
            "kind": kind.as_str(),
            "uri": source.uri().to_string(),
            "range": safe_lsp_range(source, data_range),
        }))
    } else {
        None
    };
    CodeActionOrCommand::CodeAction(CodeAction {
        title,
        kind: Some(kind),
        diagnostics: diagnostic.map(|diagnostic| vec![to_lsp_diagnostic(source, diagnostic)]),
        edit: edit.cloned(),
        command: None,
        is_preferred: Some(preferred),
        disabled: None,
        data,
    })
}

fn to_lsp_diagnostic(source: &SourceFile, diagnostic: &AnalysisDiagnostic) -> Diagnostic {
    Diagnostic {
        range: safe_lsp_range(source, diagnostic.range),
        severity: Some(match diagnostic.severity {
            DiagnosticSeverity::Error => LspDiagnosticSeverity::ERROR,
            DiagnosticSeverity::Warning => LspDiagnosticSeverity::WARNING,
            DiagnosticSeverity::Information => LspDiagnosticSeverity::INFORMATION,
            DiagnosticSeverity::Hint => LspDiagnosticSeverity::HINT,
        }),
        code: Some(NumberOrString::String(diagnostic.code.clone())),
        code_description: None,
        source: Some("hardroc".to_string()),
        message: diagnostic.message.clone(),
        related_information: None,
        tags: None,
        data: None,
    }
}

fn workspace_edit_for_fix(
    ctx: &FeatureContext<'_>,
    edits: &[SuggestedEdit],
) -> Option<WorkspaceEdit> {
    if edits.is_empty() {
        return None;
    }
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    let mut seen = HashSet::new();

    for edit in edits {
        let source = source_for_uri(ctx, &edit.uri)?;
        let range = edit.range.clamp_to(source.len());
        if !source.text().is_char_boundary(range.start as usize)
            || !source.text().is_char_boundary(range.end as usize)
        {
            return None;
        }
        let lsp_range = source.range(range);
        let key = format!(
            "{}\u{0}{}\u{0}{}\u{0}{}",
            lsp_range.start.line,
            lsp_range.start.character,
            lsp_range.end.line,
            lsp_range.end.character
        );
        if !seen.insert(key) {
            continue;
        }
        changes.entry(edit.uri.clone()).or_default().push(TextEdit {
            range: lsp_range,
            new_text: edit.new_text.clone(),
        });
    }

    if changes.is_empty() {
        return None;
    }
    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

fn source_for_uri(ctx: &FeatureContext<'_>, uri: &Url) -> Option<Arc<SourceFile>> {
    if uri == &ctx.analysis.uri {
        return Some(Arc::clone(&ctx.analysis.source));
    }
    if let Some(analysis) = ctx.host.analyze(uri) {
        return Some(Arc::clone(&analysis.source));
    }
    let path = uri.to_file_path().ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    Some(Arc::new(SourceFile::new(uri.clone(), text)))
}

fn safe_lsp_range(source: &SourceFile, range: TextRange) -> Range {
    let mut range = range.clamp_to(source.len());
    let text = source.text();
    while range.start > 0 && !text.is_char_boundary(range.start as usize) {
        range.start -= 1;
    }
    while (range.end as usize) < text.len() && !text.is_char_boundary(range.end as usize) {
        range.end += 1;
    }
    source.range(range)
}

fn single_file_edit(
    source: &SourceFile,
    uri: &Url,
    range: TextRange,
    new_text: String,
) -> WorkspaceEdit {
    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: safe_lsp_range(source, range),
            new_text,
        }],
    );
    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

fn ranges_intersect(left: TextRange, right: TextRange) -> bool {
    if left.is_empty() && right.is_empty() {
        return left.start == right.start;
    }
    if left.is_empty() {
        return right.contains_inclusive(left.start);
    }
    if right.is_empty() {
        return left.contains_inclusive(right.start);
    }
    left.intersects(right)
        || left.contains_inclusive(right.start)
        || right.contains_inclusive(left.start)
}

fn word_at(ctx: &FeatureContext<'_>, offset: u32) -> Option<(String, TextRange)> {
    if let Some(word) = ctx.analysis.word_at(offset) {
        return Some(word);
    }
    crate::analysis::identifier_at(&ctx.analysis.source, offset)
}

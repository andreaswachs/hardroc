//! Structure: symbols, folding, selection ranges, semantic tokens, links, and
//! code lenses.
//!
//! Everything here is derived from the analysis layer.  The syntax tree and
//! token stream are only consulted for structure that the symbol table does not
//! model (expects, import blocks, comments, literal spans), and every range is
//! converted through [`SourceFile`] so UTF-16 positions stay correct.

#![allow(deprecated)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::{
    CodeLens, CodeLensParams, Command, DocumentLink, DocumentLinkParams, DocumentSymbol,
    DocumentSymbolParams, DocumentSymbolResponse, FoldingRange, FoldingRangeKind,
    FoldingRangeParams, Location, Range, SelectionRange, SelectionRangeParams, SemanticToken,
    SemanticTokens, SemanticTokensParams, SemanticTokensRangeParams, SymbolInformation,
    SymbolKind as LspSymbolKind, Url, WorkspaceSymbolParams,
};

use crate::analysis::index::Export;
use crate::analysis::packages::roc_cache_dir;
use crate::analysis::{
    AnalyzedFile, DefinitionKey, ModuleSummary, ResolvedImport, SymbolInfo, SymbolKind,
    WorkspaceIndex,
};
use crate::syntax::ast::{
    Dependency, DependencySource, Expr, ExprKind, ImportPath, Item, ItemKind, MatchBranch, Pattern,
    PatternKind, Stmt, StmtKind, TypeDecl, TypeExpr, TypeKind,
};
use crate::syntax::token::{Token, TokenKind};
use crate::text::{path_to_uri, SourceFile, TextRange};

use super::util;
use super::FeatureContext;

// ---------------------------------------------------------------------------
// Document symbols
// ---------------------------------------------------------------------------

struct Outline {
    name: String,
    detail: Option<String>,
    kind: LspSymbolKind,
    name_range: TextRange,
    full_range: TextRange,
    children: Vec<Outline>,
}

pub fn document_symbols(
    ctx: &FeatureContext<'_>,
    _params: &DocumentSymbolParams,
) -> Option<DocumentSymbolResponse> {
    let analysis = ctx.analysis;
    let source = &*analysis.source;
    let len = source.len();

    let mut children: Vec<Outline> = analysis
        .symbols
        .iter()
        .map(|symbol| outline_from_symbol(analysis, symbol))
        .collect();

    merge_module_items(analysis, &analysis.module().items, &mut children);
    sort_outlines(&mut children);

    let module = analysis.module();
    let full_range = TextRange::new(0, len);
    let selection = module
        .header
        .as_ref()
        .map(|header| header.range)
        .filter(|range| !range.is_empty())
        .unwrap_or(full_range);

    let name = if analysis.module_name.is_empty() {
        "module".to_string()
    } else {
        analysis.module_name.clone()
    };
    let root = Outline {
        name,
        detail: Some(analysis.module_kind.as_str().to_string()),
        kind: LspSymbolKind::MODULE,
        name_range: selection,
        full_range,
        children,
    };

    Some(DocumentSymbolResponse::Nested(vec![
        outline_to_document_symbol(source, root),
    ]))
}

fn outline_from_symbol(analysis: &AnalyzedFile, symbol: &SymbolInfo) -> Outline {
    let full_range = if symbol.full_range.is_empty() {
        symbol.name_range
    } else {
        symbol.full_range
    };
    Outline {
        name: symbol.name.clone(),
        detail: symbol.detail.clone().or_else(|| {
            analysis
                .signature_for(&symbol.key)
                .map(|sig| sig.display.clone())
        }),
        kind: util::symbol_kind(symbol.kind),
        name_range: symbol.name_range,
        full_range,
        children: symbol
            .children
            .iter()
            .map(|child| outline_from_symbol(analysis, child))
            .collect(),
    }
}

#[allow(deprecated)]
fn outline_to_document_symbol(source: &SourceFile, node: Outline) -> DocumentSymbol {
    let len = source.len();
    let mut full = node.full_range.clamp_to(len);
    let selection = node.name_range.clamp_to(len);
    if full.is_empty() {
        full = selection;
    }
    if selection.start < full.start || selection.end > full.end {
        full = full.union(selection);
    }
    let children = if node.children.is_empty() {
        None
    } else {
        Some(
            node.children
                .into_iter()
                .map(|child| outline_to_document_symbol(source, child))
                .collect(),
        )
    };
    DocumentSymbol {
        name: node.name,
        detail: node.detail.filter(|detail| !detail.trim().is_empty()),
        kind: node.kind,
        tags: None,
        deprecated: None,
        range: source.range(full),
        selection_range: source.range(selection),
        children,
    }
}

fn sort_outlines(nodes: &mut Vec<Outline>) {
    nodes.sort_by_key(|node| (node.full_range.start, node.name_range.start));
    for node in nodes {
        sort_outlines(&mut node.children);
    }
}

fn merge_module_items(analysis: &AnalyzedFile, items: &[Item], output: &mut Vec<Outline>) {
    for item in items {
        match &item.kind {
            ItemKind::Definition(definition) => {
                for outline in definition_outlines(analysis, definition, item.range) {
                    if !output
                        .iter()
                        .any(|existing| existing.name_range == outline.name_range)
                    {
                        output.push(outline);
                    }
                }
            }
            ItemKind::TypeDecl(decl) => merge_type_decl(analysis, decl, item.range, output),
            ItemKind::WhereAlias(alias) => {
                output.push(Outline {
                    name: alias.name.text.clone(),
                    detail: Some(collapse_whitespace(analysis.source.slice(alias.range))),
                    kind: LspSymbolKind::STRUCT,
                    name_range: alias.name.range,
                    full_range: item.range,
                    children: Vec::new(),
                });
            }
            ItemKind::Expect(expect) => {
                let already = output.iter().any(|existing| {
                    existing.full_range.start == item.range.start
                        && existing.name.starts_with("expect")
                });
                if already {
                    continue;
                }
                let source = &*analysis.source;
                let text = collapse_whitespace(source.slice(expect.expr.range));
                let snippet = truncate_chars(&text, 40);
                let name = if snippet.is_empty() {
                    "expect".to_string()
                } else {
                    format!("expect {snippet}")
                };
                let name_range = if expect.expr.range.is_empty() {
                    item.range
                } else {
                    expect.expr.range
                };
                output.push(Outline {
                    name,
                    detail: None,
                    kind: LspSymbolKind::FUNCTION,
                    name_range,
                    full_range: item.range,
                    children: Vec::new(),
                });
            }
            ItemKind::Annotation(annotation) => {
                if output
                    .iter()
                    .any(|existing| existing.name_range == annotation.name.range)
                {
                    continue;
                }
                output.push(Outline {
                    name: annotation.name.text.clone(),
                    detail: Some(collapse_whitespace(
                        analysis.source.slice(annotation.ty.range),
                    )),
                    kind: LspSymbolKind::VARIABLE,
                    name_range: annotation.name.range,
                    full_range: item.range,
                    children: Vec::new(),
                });
            }
            ItemKind::Error => {}
        }
    }
}

fn merge_type_decl(
    analysis: &AnalyzedFile,
    decl: &TypeDecl,
    item_range: TextRange,
    output: &mut Vec<Outline>,
) {
    let mut children = Vec::new();
    merge_module_items(analysis, &decl.members, &mut children);
    sort_outlines(&mut children);

    if let Some(existing) = output
        .iter_mut()
        .find(|node| node.name_range == decl.name.range)
    {
        for child in children {
            if !existing
                .children
                .iter()
                .any(|node| node.name_range == child.name_range)
            {
                existing.children.push(child);
            }
        }
        sort_outlines(&mut existing.children);
        return;
    }

    let detail = {
        let text = collapse_whitespace(analysis.source.slice(decl.backing.range));
        if text.is_empty() {
            Some(type_decl_keyword(decl).to_string())
        } else {
            Some(truncate_chars(&text, 80))
        }
    };
    output.push(Outline {
        name: decl.name.text.clone(),
        detail,
        kind: LspSymbolKind::STRUCT,
        name_range: decl.name.range,
        full_range: item_range,
        children,
    });
}

fn type_decl_keyword(decl: &TypeDecl) -> &'static str {
    match decl.kind {
        crate::syntax::ast::TypeDeclKind::Alias => "alias",
        crate::syntax::ast::TypeDeclKind::Nominal => "nominal type",
        crate::syntax::ast::TypeDeclKind::Opaque => "opaque type",
    }
}

fn definition_outlines(
    analysis: &AnalyzedFile,
    definition: &crate::syntax::ast::Definition,
    item_range: TextRange,
) -> Vec<Outline> {
    let mut bindings: Vec<(String, TextRange)> = Vec::new();
    if let Some(name) = definition.simple_name() {
        if name.text != "_" {
            bindings.push((name.text.clone(), name.range));
        }
    } else {
        let mut found = Vec::new();
        definition.pattern.bindings(&mut found);
        for (name, range, _) in found {
            if name != "_" {
                bindings.push((name, range));
            }
        }
    }

    let kind = if matches!(&definition.value.kind, ExprKind::Lambda { .. }) {
        LspSymbolKind::FUNCTION
    } else if definition.is_var {
        LspSymbolKind::VARIABLE
    } else {
        LspSymbolKind::CONSTANT
    };
    let detail = definition
        .annotation
        .as_ref()
        .map(|ty| collapse_whitespace(analysis.source.slice(ty.range)))
        .filter(|text| !text.is_empty());
    let full_range = if definition.range.is_empty() {
        item_range
    } else {
        definition.range
    };

    bindings
        .into_iter()
        .map(|(name, name_range)| Outline {
            name,
            detail: detail.clone(),
            kind,
            name_range,
            full_range,
            children: Vec::new(),
        })
        .collect()
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let prefix: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{prefix}...")
}

// ---------------------------------------------------------------------------
// Workspace symbols
// ---------------------------------------------------------------------------

const MAX_WORKSPACE_SYMBOLS: usize = 5000;

pub fn workspace_symbols(
    ctx: &FeatureContext<'_>,
    params: &WorkspaceSymbolParams,
) -> Vec<SymbolInformation> {
    let analyses = ctx.host.all_analyses();
    let mut by_uri: HashMap<Url, Arc<AnalyzedFile>> = HashMap::new();
    for analysis in &analyses {
        by_uri.insert(analysis.uri.clone(), Arc::clone(analysis));
    }

    let workspace = ctx.host.workspace();
    let mut output: Vec<SymbolInformation> = Vec::new();
    let mut seen: HashSet<(Url, String, u32, u32)> = HashSet::new();
    let mut summarized: HashSet<Url> = HashSet::new();

    for module in &workspace.modules {
        summarized.insert(module.uri.clone());
        let source = by_uri
            .get(&module.uri)
            .map(|analysis| Arc::clone(&analysis.source));
        let package = module.package.clone().filter(|package| !package.is_empty());
        let module_container = match &package {
            Some(package) => format!("{package}.{}", module.name),
            None => module.name.clone(),
        };

        let range = summary_range(source.as_deref(), module);
        push_symbol_information(
            &mut output,
            &mut seen,
            SymbolInformation {
                name: module.name.clone(),
                kind: LspSymbolKind::MODULE,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: module.uri.clone(),
                    range,
                },
                container_name: package.clone(),
            },
        );

        for export in &module.exports {
            push_export(
                &mut output,
                &mut seen,
                export,
                source.as_deref(),
                &module.uri,
                &module_container,
            );
        }
    }

    for (uri, analysis) in &by_uri {
        if summarized.contains(uri) {
            continue;
        }
        push_analysis_module(&mut output, &mut seen, analysis, None);
    }

    if !summarized.contains(&ctx.analysis.uri) && !by_uri.contains_key(&ctx.analysis.uri) {
        push_analysis_module(&mut output, &mut seen, ctx.analysis, None);
    }

    let query = params.query.trim().to_lowercase();
    let mut scored: Vec<(u8, String, String, SymbolInformation)> = output
        .into_iter()
        .filter_map(|info| {
            let container = info.container_name.clone().unwrap_or_default();
            match match_quality(&query, &info.name, &container) {
                Some(score) => {
                    let name_key = info.name.to_lowercase();
                    let container_key = container.to_lowercase();
                    Some((score, name_key, container_key, info))
                }
                None => None,
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    scored
        .into_iter()
        .take(MAX_WORKSPACE_SYMBOLS)
        .map(|(_, _, _, info)| info)
        .collect()
}

fn summary_range(source: Option<&SourceFile>, module: &ModuleSummary) -> Range {
    let range = module
        .header_range
        .filter(|range| !range.is_empty())
        .unwrap_or_else(|| TextRange::empty(0));
    source.map(|source| source.range(range)).unwrap_or_default()
}

fn push_export(
    output: &mut Vec<SymbolInformation>,
    seen: &mut HashSet<(Url, String, u32, u32)>,
    export: &Export,
    source: Option<&SourceFile>,
    uri: &Url,
    container: &str,
) {
    let range = source
        .map(|source| source.range(export.selection_range))
        .unwrap_or_default();
    push_symbol_information(
        output,
        seen,
        SymbolInformation {
            name: export.name.clone(),
            kind: util::symbol_kind(export.kind),
            tags: None,
            deprecated: None,
            location: Location {
                uri: uri.clone(),
                range,
            },
            container_name: Some(container.to_string()),
        },
    );
    let child_container = format!("{container}.{}", export.name);
    for child in &export.children {
        push_export(output, seen, child, source, uri, &child_container);
    }
}

fn push_analysis_module(
    output: &mut Vec<SymbolInformation>,
    seen: &mut HashSet<(Url, String, u32, u32)>,
    analysis: &AnalyzedFile,
    package: Option<String>,
) {
    let source = &*analysis.source;
    let module_range = if analysis.module().range.is_empty() {
        TextRange::empty(0)
    } else {
        analysis.module().range
    };
    let container = package.clone();
    let module_container = match &package {
        Some(package) => format!("{package}.{}", analysis.module_name),
        None => analysis.module_name.clone(),
    };
    push_symbol_information(
        output,
        seen,
        SymbolInformation {
            name: if analysis.module_name.is_empty() {
                "module".to_string()
            } else {
                analysis.module_name.clone()
            },
            kind: LspSymbolKind::MODULE,
            tags: None,
            deprecated: None,
            location: Location {
                uri: analysis.uri.clone(),
                range: source.range(module_range),
            },
            container_name: container,
        },
    );
    push_analysis_symbols(
        output,
        seen,
        source,
        &analysis.uri,
        &analysis.symbols,
        &module_container,
    );
}

fn push_analysis_symbols(
    output: &mut Vec<SymbolInformation>,
    seen: &mut HashSet<(Url, String, u32, u32)>,
    source: &SourceFile,
    uri: &Url,
    symbols: &[SymbolInfo],
    container: &str,
) {
    for symbol in symbols {
        push_symbol_information(
            output,
            seen,
            SymbolInformation {
                name: symbol.name.clone(),
                kind: util::symbol_kind(symbol.kind),
                tags: None,
                deprecated: None,
                location: Location {
                    uri: uri.clone(),
                    range: source.range(symbol.name_range),
                },
                container_name: Some(container.to_string()),
            },
        );
        let child_container = format!("{container}.{}", symbol.name);
        push_analysis_symbols(
            output,
            seen,
            source,
            uri,
            &symbol.children,
            &child_container,
        );
    }
}

fn push_symbol_information(
    output: &mut Vec<SymbolInformation>,
    seen: &mut HashSet<(Url, String, u32, u32)>,
    info: SymbolInformation,
) {
    let key = (
        info.location.uri.clone(),
        info.name.clone(),
        info.location.range.start.line,
        info.location.range.start.character,
    );
    if seen.insert(key) {
        output.push(info);
    }
}

/// Rank a symbol against the query.  Lower is better: exact, prefix,
/// substring, no-query, fuzzy subsequence.
fn match_quality(query: &str, name: &str, container: &str) -> Option<u8> {
    let name = name.to_lowercase();
    if query.is_empty() {
        return Some(3);
    }
    if name == query {
        return Some(0);
    }
    if name.starts_with(query) {
        return Some(1);
    }
    if name.contains(query) {
        return Some(2);
    }
    if !container.is_empty() {
        let qualified = format!("{}.{}", container.to_lowercase(), name);
        if qualified == query {
            return Some(0);
        }
        if qualified.starts_with(query) {
            return Some(1);
        }
        if qualified.contains(query) {
            return Some(2);
        }
    }
    if query.chars().count() > 1 && is_subsequence(&name, query) {
        return Some(4);
    }
    None
}

fn is_subsequence(text: &str, needle: &str) -> bool {
    let mut chars = needle.chars();
    let mut wanted = chars.next();
    for ch in text.chars() {
        if Some(ch) == wanted {
            wanted = chars.next();
            if wanted.is_none() {
                return true;
            }
        }
    }
    wanted.is_none()
}

// ---------------------------------------------------------------------------
// Folding ranges
// ---------------------------------------------------------------------------

pub fn folding_ranges(ctx: &FeatureContext<'_>, _params: &FoldingRangeParams) -> Vec<FoldingRange> {
    let analysis = ctx.analysis;
    let source = &*analysis.source;
    let module = analysis.module();
    let mut folds: Vec<FoldingRange> = Vec::new();
    let mut seen: HashSet<(u32, u32)> = HashSet::new();

    if let Some(header) = &module.header {
        if !header.dependencies.is_empty() {
            add_fold(
                &mut folds,
                &mut seen,
                source,
                header.range,
                Some(FoldingRangeKind::Region),
            );
        }
    }

    if let Some(first) = module.imports.first() {
        if let Some(last) = module.imports.last() {
            add_fold(
                &mut folds,
                &mut seen,
                source,
                first.range.union(last.range),
                Some(FoldingRangeKind::Imports),
            );
        }
        for import in &module.imports {
            add_fold(&mut folds, &mut seen, source, import.range, None);
        }
    }

    collect_token_folds(&analysis.parsed.tokens, source, &mut folds, &mut seen);
    fold_items(&module.items, source, &mut folds, &mut seen);

    folds.sort_by_key(|fold| (fold.start_line, fold.end_line));
    folds
}

fn add_fold(
    folds: &mut Vec<FoldingRange>,
    seen: &mut HashSet<(u32, u32)>,
    source: &SourceFile,
    range: TextRange,
    kind: Option<FoldingRangeKind>,
) {
    let range = range.clamp_to(source.len());
    if range.is_empty() {
        return;
    }
    let start_line = source.line_of_offset(range.start) as u32;
    let end_line = source.line_of_offset(range.end.saturating_sub(1)) as u32;
    if end_line <= start_line {
        return;
    }
    if !seen.insert((start_line, end_line)) {
        return;
    }
    folds.push(FoldingRange {
        start_line,
        start_character: None,
        end_line,
        end_character: None,
        kind,
        collapsed_text: None,
    });
}

fn collect_token_folds(
    tokens: &[Token],
    source: &SourceFile,
    folds: &mut Vec<FoldingRange>,
    seen: &mut HashSet<(u32, u32)>,
) {
    let mut block_stack: Vec<(TokenKind, TextRange)> = Vec::new();
    let mut doc_run: Option<(u32, u32)> = None;
    let len = source.len();

    for token in tokens {
        let range = token.range.clamp_to(len);
        match token.kind {
            TokenKind::Whitespace | TokenKind::Newline => continue,
            TokenKind::DocComment => {
                if !range.is_empty() {
                    let start = source.line_of_offset(range.start) as u32;
                    let end = source.line_of_offset(range.end.saturating_sub(1)) as u32;
                    doc_run = match doc_run {
                        Some((run_start, run_end)) if start <= run_end.saturating_add(1) => {
                            Some((run_start, run_end.max(end)))
                        }
                        _ => Some((start, end)),
                    };
                    if end > start {
                        add_fold(folds, seen, source, range, Some(FoldingRangeKind::Comment));
                    }
                }
            }
            TokenKind::Comment => {
                flush_doc_run(&mut doc_run, folds, seen);
                if source.line_of_offset(range.start)
                    < source.line_of_offset(range.end.saturating_sub(1))
                {
                    add_fold(folds, seen, source, range, Some(FoldingRangeKind::Comment));
                }
            }
            TokenKind::MultilineString => {
                flush_doc_run(&mut doc_run, folds, seen);
                add_fold(folds, seen, source, range, None);
            }
            TokenKind::LBrace | TokenKind::LBracket => {
                flush_doc_run(&mut doc_run, folds, seen);
                block_stack.push((token.kind, range));
            }
            TokenKind::RBrace => {
                flush_doc_run(&mut doc_run, folds, seen);
                close_block(
                    &mut block_stack,
                    TokenKind::LBrace,
                    range,
                    source,
                    folds,
                    seen,
                );
            }
            TokenKind::RBracket => {
                flush_doc_run(&mut doc_run, folds, seen);
                close_block(
                    &mut block_stack,
                    TokenKind::LBracket,
                    range,
                    source,
                    folds,
                    seen,
                );
            }
            _ => flush_doc_run(&mut doc_run, folds, seen),
        }
    }
    flush_doc_run(&mut doc_run, folds, seen);
}

fn flush_doc_run(
    doc_run: &mut Option<(u32, u32)>,
    folds: &mut Vec<FoldingRange>,
    seen: &mut HashSet<(u32, u32)>,
) {
    let Some((start, end)) = doc_run.take() else {
        return;
    };
    if end > start && seen.insert((start, end)) {
        folds.push(FoldingRange {
            start_line: start,
            start_character: None,
            end_line: end,
            end_character: None,
            kind: Some(FoldingRangeKind::Comment),
            collapsed_text: None,
        });
    }
}

fn close_block(
    stack: &mut Vec<(TokenKind, TextRange)>,
    opener: TokenKind,
    close_range: TextRange,
    source: &SourceFile,
    folds: &mut Vec<FoldingRange>,
    seen: &mut HashSet<(u32, u32)>,
) {
    if stack.last().map(|(kind, _)| *kind) != Some(opener) {
        return;
    }
    if let Some((_, open_range)) = stack.pop() {
        add_fold(folds, seen, source, open_range.union(close_range), None);
    }
}

fn fold_items(
    items: &[Item],
    source: &SourceFile,
    folds: &mut Vec<FoldingRange>,
    seen: &mut HashSet<(u32, u32)>,
) {
    for item in items {
        match &item.kind {
            ItemKind::Definition(definition) => {
                add_fold(folds, seen, source, definition.range, None);
                fold_expr(&definition.value, source, folds, seen);
            }
            ItemKind::TypeDecl(decl) => {
                add_fold(folds, seen, source, item.range, None);
                fold_type(&decl.backing, source, folds, seen);
                fold_items(&decl.members, source, folds, seen);
            }
            ItemKind::WhereAlias(alias) => {
                add_fold(folds, seen, source, item.range, None);
                for requirement in &alias.requirements {
                    fold_type(&requirement.ty, source, folds, seen);
                }
            }
            ItemKind::Expect(expect) => {
                add_fold(folds, seen, source, item.range, None);
                fold_expr(&expect.expr, source, folds, seen);
            }
            ItemKind::Annotation(annotation) => {
                add_fold(folds, seen, source, item.range, None);
                fold_type(&annotation.ty, source, folds, seen);
            }
            ItemKind::Error => {}
        }
    }
}

fn fold_expr(
    expr: &Expr,
    source: &SourceFile,
    folds: &mut Vec<FoldingRange>,
    seen: &mut HashSet<(u32, u32)>,
) {
    match &expr.kind {
        ExprKind::Block { stmts, tail } => {
            add_fold(folds, seen, source, expr.range, None);
            for stmt in stmts {
                fold_stmt(stmt, source, folds, seen);
            }
            if let Some(tail) = tail {
                fold_expr(tail, source, folds, seen);
            }
        }
        ExprKind::Lambda { params, body } => {
            add_fold(folds, seen, source, expr.range, None);
            for param in params {
                fold_pattern(param, source, folds, seen);
            }
            fold_expr(body, source, folds, seen);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            add_fold(folds, seen, source, expr.range, None);
            add_fold(folds, seen, source, then_branch.range, None);
            add_fold(folds, seen, source, else_branch.range, None);
            fold_expr(condition, source, folds, seen);
            fold_expr(then_branch, source, folds, seen);
            fold_expr(else_branch, source, folds, seen);
        }
        ExprKind::Match { subject, branches } => {
            add_fold(folds, seen, source, expr.range, None);
            fold_expr(subject, source, folds, seen);
            for branch in branches {
                fold_match_branch(branch, source, folds, seen);
            }
        }
        ExprKind::List(items) | ExprKind::Tuple(items) => {
            add_fold(folds, seen, source, expr.range, None);
            for item in items {
                fold_expr(item, source, folds, seen);
            }
        }
        ExprKind::Record { fields, update } => {
            add_fold(folds, seen, source, expr.range, None);
            for field in fields {
                if let Some(value) = &field.value {
                    fold_expr(value, source, folds, seen);
                }
                if let Some(default) = &field.default {
                    fold_expr(default, source, folds, seen);
                }
            }
            if let Some(update) = update {
                fold_expr(update, source, folds, seen);
            }
        }
        ExprKind::Tag { args, .. } => {
            for arg in args {
                fold_expr(arg, source, folds, seen);
            }
        }
        ExprKind::Call { callee, args } => {
            fold_expr(callee, source, folds, seen);
            for arg in args {
                fold_expr(arg, source, folds, seen);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            fold_expr(receiver, source, folds, seen);
            for arg in args {
                fold_expr(arg, source, folds, seen);
            }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            fold_expr(lhs, source, folds, seen);
            fold_expr(rhs, source, folds, seen);
        }
        ExprKind::Unary { expr, .. } => fold_expr(expr, source, folds, seen),
        ExprKind::Paren(inner) => fold_expr(inner, source, folds, seen),
        ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => {
            fold_expr(base, source, folds, seen)
        }
        ExprKind::Nominal { value, .. } => fold_expr(value, source, folds, seen),
        ExprKind::For {
            pattern,
            iterable,
            body,
        } => {
            fold_pattern(pattern, source, folds, seen);
            fold_expr(iterable, source, folds, seen);
            fold_expr(body, source, folds, seen);
        }
        ExprKind::While { condition, body } => {
            add_fold(folds, seen, source, expr.range, None);
            fold_expr(condition, source, folds, seen);
            fold_expr(body, source, folds, seen);
        }
        ExprKind::Return(Some(value)) | ExprKind::Crash(value) | ExprKind::Db(value) => {
            fold_expr(value, source, folds, seen)
        }
        ExprKind::Return(None)
        | ExprKind::Break
        | ExprKind::Continue
        | ExprKind::Number(_)
        | ExprKind::String(_)
        | ExprKind::MultilineString(_)
        | ExprKind::Char(_)
        | ExprKind::Lookup { .. }
        | ExprKind::Error => {}
    }
}

fn fold_match_branch(
    branch: &MatchBranch,
    source: &SourceFile,
    folds: &mut Vec<FoldingRange>,
    seen: &mut HashSet<(u32, u32)>,
) {
    add_fold(folds, seen, source, branch.range, None);
    for pattern in &branch.patterns {
        fold_pattern(pattern, source, folds, seen);
    }
    if let Some(guard) = &branch.guard {
        fold_expr(guard, source, folds, seen);
    }
    fold_expr(&branch.body, source, folds, seen);
}

fn fold_stmt(
    stmt: &Stmt,
    source: &SourceFile,
    folds: &mut Vec<FoldingRange>,
    seen: &mut HashSet<(u32, u32)>,
) {
    add_fold(folds, seen, source, stmt.range, None);
    match &stmt.kind {
        StmtKind::Definition(definition) => fold_expr(&definition.value, source, folds, seen),
        StmtKind::TypeDecl(_) => {}
        StmtKind::Assignment { pattern, value } => {
            fold_pattern(pattern, source, folds, seen);
            fold_expr(value, source, folds, seen);
        }
        StmtKind::Db(value) | StmtKind::Return(Some(value)) | StmtKind::Crash(value) => {
            fold_expr(value, source, folds, seen)
        }
        StmtKind::Expect(value) | StmtKind::Expr(value) => fold_expr(value, source, folds, seen),
        StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
    }
}

fn fold_pattern(
    pattern: &Pattern,
    source: &SourceFile,
    folds: &mut Vec<FoldingRange>,
    seen: &mut HashSet<(u32, u32)>,
) {
    add_fold(folds, seen, source, pattern.range, None);
    match &pattern.kind {
        PatternKind::Tag { args, .. }
        | PatternKind::Tuple(args)
        | PatternKind::Alternatives(args) => {
            for arg in args {
                fold_pattern(arg, source, folds, seen);
            }
        }
        PatternKind::Record { fields, .. } => {
            for field in fields {
                if let Some(pattern) = &field.pattern {
                    fold_pattern(pattern, source, folds, seen);
                }
            }
        }
        PatternKind::List { elements, .. } => {
            for element in elements {
                fold_pattern(element, source, folds, seen);
            }
        }
        PatternKind::As { pattern, .. } | PatternKind::Nominal { pattern, .. } => {
            fold_pattern(pattern, source, folds, seen)
        }
        PatternKind::Name { .. }
        | PatternKind::Wildcard
        | PatternKind::Number(_)
        | PatternKind::String(_)
        | PatternKind::Char(_)
        | PatternKind::Error => {}
    }
}

fn fold_type(
    ty: &TypeExpr,
    source: &SourceFile,
    folds: &mut Vec<FoldingRange>,
    seen: &mut HashSet<(u32, u32)>,
) {
    match &ty.kind {
        TypeKind::Function { args, ret, .. } => {
            for arg in args {
                fold_type(arg, source, folds, seen);
            }
            fold_type(ret, source, folds, seen);
        }
        TypeKind::Tuple(elements) => {
            add_fold(folds, seen, source, ty.range, None);
            for element in elements {
                fold_type(element, source, folds, seen);
            }
        }
        TypeKind::Record { fields, .. } => {
            add_fold(folds, seen, source, ty.range, None);
            for field in fields {
                fold_type(&field.ty, source, folds, seen);
                if let Some(default) = &field.default {
                    fold_expr(default, source, folds, seen);
                }
            }
        }
        TypeKind::TagUnion { tags, .. } => {
            add_fold(folds, seen, source, ty.range, None);
            for tag in tags {
                add_fold(folds, seen, source, tag.range, None);
                for payload in &tag.payloads {
                    fold_type(payload, source, folds, seen);
                }
            }
        }
        TypeKind::Apply { args, .. } => {
            for arg in args {
                fold_type(arg, source, folds, seen);
            }
        }
        TypeKind::Where { base, requirements } => {
            fold_type(base, source, folds, seen);
            for requirement in requirements {
                fold_type(&requirement.ty, source, folds, seen);
            }
        }
        TypeKind::Named(_) | TypeKind::Var(_) | TypeKind::Wildcard | TypeKind::Error => {}
    }
}

// ---------------------------------------------------------------------------
// Selection ranges
// ---------------------------------------------------------------------------

pub fn selection_ranges(
    ctx: &FeatureContext<'_>,
    params: &SelectionRangeParams,
) -> Vec<SelectionRange> {
    let analysis = ctx.analysis;
    let source = &*analysis.source;
    let len = source.len();

    params
        .positions
        .iter()
        .map(|position| {
            let offset = source.offset_at(*position);
            selection_chain(analysis, offset, len)
        })
        .collect()
}

fn selection_chain(analysis: &AnalyzedFile, offset: u32, len: u32) -> SelectionRange {
    let source = &*analysis.source;
    let mut candidates: Vec<TextRange> = Vec::new();
    candidates.push(TextRange::empty(offset));

    if let Some(token) = analysis.token_at(offset) {
        candidates.push(token.range);
    }
    if let Some((_, range)) = analysis.word_at(offset) {
        candidates.push(range);
    }
    if let Some(reference) = analysis.reference_at(offset) {
        candidates.push(reference.range);
    }
    if let Some(symbol) = analysis.symbol_at(offset) {
        candidates.push(symbol.name_range);
    }
    for scope in &analysis.scopes {
        if scope.range.contains_inclusive(offset) {
            candidates.push(scope.range);
        }
    }

    let module = analysis.module();
    collect_item_ranges(&module.items, offset, &mut candidates);
    if !module.range.is_empty() {
        candidates.push(module.range);
    }
    candidates.push(util::line_range(source, offset));
    candidates.push(TextRange::new(0, len));

    let mut sorted: Vec<TextRange> = candidates
        .into_iter()
        .map(|range| range.clamp_to(len))
        .filter(|range| range.contains_inclusive(offset))
        .collect();
    sorted.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| b.start.cmp(&a.start)));

    let mut accepted: Vec<TextRange> = Vec::new();
    for range in sorted {
        match accepted.last() {
            Some(previous) => {
                if range.start == previous.start && range.end == previous.end {
                    continue;
                }
                if range.start <= previous.start && range.end >= previous.end {
                    accepted.push(range);
                }
            }
            None => accepted.push(range),
        }
    }

    let mut node: Option<SelectionRange> = None;
    for range in accepted.into_iter().rev() {
        node = Some(SelectionRange {
            range: source.range(range),
            parent: node.map(Box::new),
        });
    }
    node.unwrap_or_else(|| SelectionRange {
        range: source.range(TextRange::empty(offset)),
        parent: Some(Box::new(SelectionRange {
            range: source.range(TextRange::new(0, len)),
            parent: None,
        })),
    })
}

fn collect_item_ranges(items: &[Item], offset: u32, output: &mut Vec<TextRange>) {
    for item in items {
        if !item.range.contains_inclusive(offset) {
            continue;
        }
        output.push(item.range);
        match &item.kind {
            ItemKind::Definition(definition) => {
                collect_pattern_ranges(&definition.pattern, offset, output);
                if let Some(annotation) = &definition.annotation {
                    collect_type_ranges(annotation, offset, output);
                }
                collect_expr_ranges(&definition.value, offset, output);
            }
            ItemKind::TypeDecl(decl) => {
                collect_type_ranges(&decl.backing, offset, output);
                collect_item_ranges(&decl.members, offset, output);
            }
            ItemKind::WhereAlias(alias) => {
                for requirement in &alias.requirements {
                    collect_type_ranges(&requirement.ty, offset, output);
                }
            }
            ItemKind::Expect(expect) => collect_expr_ranges(&expect.expr, offset, output),
            ItemKind::Annotation(annotation) => collect_type_ranges(&annotation.ty, offset, output),
            ItemKind::Error => {}
        }
    }
}

fn collect_expr_ranges(expr: &Expr, offset: u32, output: &mut Vec<TextRange>) {
    if !expr.range.contains_inclusive(offset) {
        return;
    }
    output.push(expr.range);
    match &expr.kind {
        ExprKind::Tag { args, .. } => {
            for arg in args {
                collect_expr_ranges(arg, offset, output);
            }
        }
        ExprKind::List(items) | ExprKind::Tuple(items) => {
            for item in items {
                collect_expr_ranges(item, offset, output);
            }
        }
        ExprKind::Record { fields, update } => {
            for field in fields {
                if let Some(value) = &field.value {
                    collect_expr_ranges(value, offset, output);
                }
                if let Some(default) = &field.default {
                    collect_expr_ranges(default, offset, output);
                }
            }
            if let Some(update) = update {
                collect_expr_ranges(update, offset, output);
            }
        }
        ExprKind::Lambda { params, body } => {
            for param in params {
                collect_pattern_ranges(param, offset, output);
            }
            collect_expr_ranges(body, offset, output);
        }
        ExprKind::Block { stmts, tail } => {
            for stmt in stmts {
                collect_stmt_ranges(stmt, offset, output);
            }
            if let Some(tail) = tail {
                collect_expr_ranges(tail, offset, output);
            }
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_expr_ranges(condition, offset, output);
            collect_expr_ranges(then_branch, offset, output);
            collect_expr_ranges(else_branch, offset, output);
        }
        ExprKind::Match { subject, branches } => {
            collect_expr_ranges(subject, offset, output);
            for branch in branches {
                if branch.range.contains_inclusive(offset) {
                    output.push(branch.range);
                    for pattern in &branch.patterns {
                        collect_pattern_ranges(pattern, offset, output);
                    }
                    if let Some(guard) = &branch.guard {
                        collect_expr_ranges(guard, offset, output);
                    }
                    collect_expr_ranges(&branch.body, offset, output);
                }
            }
        }
        ExprKind::Lookup { .. }
        | ExprKind::Number(_)
        | ExprKind::String(_)
        | ExprKind::MultilineString(_)
        | ExprKind::Char(_)
        | ExprKind::Break
        | ExprKind::Continue
        | ExprKind::Error => {}
        ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => {
            collect_expr_ranges(base, offset, output)
        }
        ExprKind::Call { callee, args } => {
            collect_expr_ranges(callee, offset, output);
            for arg in args {
                collect_expr_ranges(arg, offset, output);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            collect_expr_ranges(receiver, offset, output);
            for arg in args {
                collect_expr_ranges(arg, offset, output);
            }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_ranges(lhs, offset, output);
            collect_expr_ranges(rhs, offset, output);
        }
        ExprKind::Unary { expr, .. }
        | ExprKind::Paren(expr)
        | ExprKind::Return(Some(expr))
        | ExprKind::Crash(expr)
        | ExprKind::Db(expr) => collect_expr_ranges(expr, offset, output),
        ExprKind::Return(None) => {}
        ExprKind::For {
            pattern,
            iterable,
            body,
        } => {
            collect_pattern_ranges(pattern, offset, output);
            collect_expr_ranges(iterable, offset, output);
            collect_expr_ranges(body, offset, output);
        }
        ExprKind::While { condition, body } => {
            collect_expr_ranges(condition, offset, output);
            collect_expr_ranges(body, offset, output);
        }
        ExprKind::Nominal { value, .. } => collect_expr_ranges(value, offset, output),
    }
}

fn collect_stmt_ranges(stmt: &Stmt, offset: u32, output: &mut Vec<TextRange>) {
    if !stmt.range.contains_inclusive(offset) {
        return;
    }
    output.push(stmt.range);
    match &stmt.kind {
        StmtKind::Definition(definition) => {
            collect_pattern_ranges(&definition.pattern, offset, output);
            if let Some(annotation) = &definition.annotation {
                collect_type_ranges(annotation, offset, output);
            }
            collect_expr_ranges(&definition.value, offset, output);
        }
        StmtKind::TypeDecl(decl) => {
            collect_type_ranges(&decl.backing, offset, output);
        }
        StmtKind::Assignment { pattern, value } => {
            collect_pattern_ranges(pattern, offset, output);
            collect_expr_ranges(value, offset, output);
        }
        StmtKind::Db(value)
        | StmtKind::Expect(value)
        | StmtKind::Return(Some(value))
        | StmtKind::Crash(value)
        | StmtKind::Expr(value) => collect_expr_ranges(value, offset, output),
        StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
    }
}

fn collect_pattern_ranges(pattern: &Pattern, offset: u32, output: &mut Vec<TextRange>) {
    if !pattern.range.contains_inclusive(offset) {
        return;
    }
    output.push(pattern.range);
    match &pattern.kind {
        PatternKind::Tag { args, .. }
        | PatternKind::Tuple(args)
        | PatternKind::Alternatives(args) => {
            for arg in args {
                collect_pattern_ranges(arg, offset, output);
            }
        }
        PatternKind::Record { fields, .. } => {
            for field in fields {
                if let Some(pattern) = &field.pattern {
                    collect_pattern_ranges(pattern, offset, output);
                }
            }
        }
        PatternKind::List { elements, .. } => {
            for element in elements {
                collect_pattern_ranges(element, offset, output);
            }
        }
        PatternKind::As { pattern, .. } | PatternKind::Nominal { pattern, .. } => {
            collect_pattern_ranges(pattern, offset, output)
        }
        PatternKind::Name { .. }
        | PatternKind::Wildcard
        | PatternKind::Number(_)
        | PatternKind::String(_)
        | PatternKind::Char(_)
        | PatternKind::Error => {}
    }
}

fn collect_type_ranges(ty: &TypeExpr, offset: u32, output: &mut Vec<TextRange>) {
    if !ty.range.contains_inclusive(offset) {
        return;
    }
    output.push(ty.range);
    match &ty.kind {
        TypeKind::Function { args, ret, .. } => {
            for arg in args {
                collect_type_ranges(arg, offset, output);
            }
            collect_type_ranges(ret, offset, output);
        }
        TypeKind::Tuple(elements) => {
            for element in elements {
                collect_type_ranges(element, offset, output);
            }
        }
        TypeKind::Record { fields, .. } => {
            for field in fields {
                collect_type_ranges(&field.ty, offset, output);
            }
        }
        TypeKind::TagUnion { tags, .. } => {
            for tag in tags {
                for payload in &tag.payloads {
                    collect_type_ranges(payload, offset, output);
                }
            }
        }
        TypeKind::Apply { args, .. } => {
            for arg in args {
                collect_type_ranges(arg, offset, output);
            }
        }
        TypeKind::Where { base, requirements } => {
            collect_type_ranges(base, offset, output);
            for requirement in requirements {
                collect_type_ranges(&requirement.ty, offset, output);
            }
        }
        TypeKind::Named(_) | TypeKind::Var(_) | TypeKind::Wildcard | TypeKind::Error => {}
    }
}

// ---------------------------------------------------------------------------
// Semantic tokens
// ---------------------------------------------------------------------------

mod token_type {
    pub const NAMESPACE: u32 = 0;
    pub const TYPE: u32 = 1;
    pub const TYPE_PARAMETER: u32 = 6;
    pub const PARAMETER: u32 = 7;
    pub const VARIABLE: u32 = 8;
    pub const PROPERTY: u32 = 9;
    pub const ENUM_MEMBER: u32 = 10;
    pub const FUNCTION: u32 = 11;
    pub const METHOD: u32 = 12;
    pub const KEYWORD: u32 = 14;
    pub const COMMENT: u32 = 16;
    pub const STRING: u32 = 17;
    pub const NUMBER: u32 = 18;
    pub const OPERATOR: u32 = 20;
}

mod modifier {
    pub const DECLARATION: u32 = 1 << 0;
    pub const READONLY: u32 = 1 << 2;
    pub const DOCUMENTATION: u32 = 1 << 8;
    pub const DEFAULT_LIBRARY: u32 = 1 << 9;
}

#[derive(Clone, Copy)]
struct PositionedToken {
    line: u32,
    column: u32,
    length: u32,
    token_type: u32,
    modifiers: u32,
}

pub fn semantic_tokens(ctx: &FeatureContext<'_>, _params: &SemanticTokensParams) -> SemanticTokens {
    semantic_tokens_impl(ctx, None)
}

pub fn semantic_tokens_range(
    ctx: &FeatureContext<'_>,
    params: &SemanticTokensRangeParams,
) -> SemanticTokens {
    let range = ctx.analysis.source.text_range(params.range);
    semantic_tokens_impl(ctx, Some(range))
}

fn semantic_tokens_impl(ctx: &FeatureContext<'_>, range: Option<TextRange>) -> SemanticTokens {
    let analysis = ctx.analysis;
    let source = &*analysis.source;

    let mut symbol_classes: HashMap<u32, (u32, u32)> = HashMap::new();
    collect_symbol_classes(&analysis.symbols, &mut symbol_classes);

    let mut reference_classes: HashMap<u32, (u32, u32)> = HashMap::new();
    for reference in &analysis.references {
        let mut modifiers = 0;
        if matches!(&reference.key, Some(DefinitionKey::Builtin { .. })) {
            modifiers |= modifier::DEFAULT_LIBRARY;
        }
        reference_classes.insert(
            reference.range.start,
            (token_type_for_symbol_kind(reference.kind), modifiers),
        );
    }

    let mut positioned: Vec<PositionedToken> = Vec::new();
    for token in &analysis.parsed.tokens {
        if let Some(range) = range {
            if !token.range.intersects(range) {
                continue;
            }
        }
        if token.kind.is_trivia()
            && !matches!(token.kind, TokenKind::Comment | TokenKind::DocComment)
        {
            continue;
        }
        let (token_type, modifiers) = match token.kind {
            TokenKind::Comment => (token_type::COMMENT, 0),
            TokenKind::DocComment => (token_type::COMMENT, modifier::DOCUMENTATION),
            TokenKind::String | TokenKind::MultilineString | TokenKind::Char => {
                (token_type::STRING, 0)
            }
            TokenKind::Number => (token_type::NUMBER, 0),
            TokenKind::Keyword => (token_type::KEYWORD, 0),
            TokenKind::Operator
            | TokenKind::Arrow
            | TokenKind::FatArrow
            | TokenKind::Pipe
            | TokenKind::Backslash => (token_type::OPERATOR, 0),
            TokenKind::LowerIdent | TokenKind::UpperIdent => {
                if let Some((token_type, symbol_modifiers)) = symbol_classes.get(&token.range.start)
                {
                    (*token_type, *symbol_modifiers)
                } else if let Some((token_type, reference_modifiers)) =
                    reference_classes.get(&token.range.start)
                {
                    (*token_type, *reference_modifiers)
                } else if token.kind == TokenKind::UpperIdent {
                    (token_type::ENUM_MEMBER, 0)
                } else {
                    (token_type::VARIABLE, 0)
                }
            }
            _ => continue,
        };
        emit_segments(source, token.range, token_type, modifiers, &mut positioned);
    }

    positioned.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.column.cmp(&b.column)));
    let mut deduped: Vec<PositionedToken> = Vec::new();
    let mut last_end: Option<(u32, u32)> = None;
    for token in positioned {
        if token.length == 0 {
            continue;
        }
        if let Some((line, end_column)) = last_end {
            if token.line == line && token.column < end_column {
                continue;
            }
        }
        last_end = Some((token.line, token.column + token.length));
        deduped.push(token);
    }

    let mut data = Vec::with_capacity(deduped.len());
    let mut previous_line = 0u32;
    let mut previous_column = 0u32;
    for token in deduped {
        let delta_line = token.line.saturating_sub(previous_line);
        let delta_start = if delta_line == 0 {
            token.column.saturating_sub(previous_column)
        } else {
            token.column
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length: token.length,
            token_type: token.token_type,
            token_modifiers_bitset: token.modifiers,
        });
        previous_line = token.line;
        previous_column = token.column;
    }

    SemanticTokens {
        result_id: None,
        data,
    }
}

fn collect_symbol_classes(symbols: &[SymbolInfo], output: &mut HashMap<u32, (u32, u32)>) {
    for symbol in symbols {
        let mut modifiers = modifier::DECLARATION;
        if matches!(symbol.kind, SymbolKind::Constant) {
            modifiers |= modifier::READONLY;
        }
        output.insert(
            symbol.name_range.start,
            (token_type_for_symbol_kind(symbol.kind), modifiers),
        );
        collect_symbol_classes(&symbol.children, output);
    }
}

fn token_type_for_symbol_kind(kind: SymbolKind) -> u32 {
    match kind {
        SymbolKind::Module | SymbolKind::Package | SymbolKind::Import => token_type::NAMESPACE,
        SymbolKind::Type => token_type::TYPE,
        SymbolKind::TypeParameter => token_type::TYPE_PARAMETER,
        SymbolKind::Parameter => token_type::PARAMETER,
        SymbolKind::Field => token_type::PROPERTY,
        SymbolKind::Tag => token_type::ENUM_MEMBER,
        SymbolKind::Function => token_type::FUNCTION,
        SymbolKind::Method => token_type::METHOD,
        SymbolKind::Constant | SymbolKind::Variable | SymbolKind::Local => token_type::VARIABLE,
    }
}

fn emit_segments(
    source: &SourceFile,
    range: TextRange,
    token_type: u32,
    modifiers: u32,
    output: &mut Vec<PositionedToken>,
) {
    let range = range.clamp_to(source.len());
    if range.is_empty() {
        return;
    }
    let start_line = source.line_of_offset(range.start);
    let end_line = source.line_of_offset(range.end.saturating_sub(1).max(range.start));
    for line in start_line..=end_line {
        let line_start = source.line_start(line);
        let line_end = source.line_end(line);
        let start = range.start.max(line_start);
        let end = range.end.min(line_end);
        if end <= start {
            continue;
        }
        let column = source.position_at(start).character;
        let end_column = source.position_at(end).character;
        if end_column <= column {
            continue;
        }
        output.push(PositionedToken {
            line: line as u32,
            column,
            length: end_column - column,
            token_type,
            modifiers,
        });
    }
}

// ---------------------------------------------------------------------------
// Document links
// ---------------------------------------------------------------------------

pub fn document_links(ctx: &FeatureContext<'_>, _params: &DocumentLinkParams) -> Vec<DocumentLink> {
    let analysis = ctx.analysis;
    let workspace = ctx.host.workspace();
    let base = analysis
        .uri
        .to_file_path()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));

    let mut links = Vec::new();
    let mut seen: HashSet<(u32, u32, String)> = HashSet::new();

    for resolved in &analysis.imports {
        let range = resolved.import.path.range();
        if range.is_empty() {
            continue;
        }
        let target = resolved
            .file_uri
            .clone()
            .or_else(|| resolved.target.as_ref().map(|target| target.uri.clone()))
            .or_else(|| resolve_import_target(resolved, &workspace, base.as_deref()));
        if let Some(target) = target {
            add_link(
                &mut links,
                &mut seen,
                analysis,
                range,
                target,
                link_tooltip("Open", &resolved.qualifier),
            );
        }
    }

    if let Some(header) = &analysis.module().header {
        for dependency in &header.dependencies {
            if dependency.source_range.is_empty() {
                continue;
            }
            if let Some(target) = dependency_target(dependency, &workspace, base.as_deref()) {
                add_link(
                    &mut links,
                    &mut seen,
                    analysis,
                    dependency.source_range,
                    target,
                    link_tooltip("Open dependency", &dependency.shorthand.text),
                );
            }
        }
    }

    links.sort_by_key(|link| (link.range.start.line, link.range.start.character));
    links
}

fn link_tooltip(action: &str, label: &str) -> String {
    if label.is_empty() {
        action.to_string()
    } else {
        format!("{action} {label}")
    }
}

fn add_link(
    links: &mut Vec<DocumentLink>,
    seen: &mut HashSet<(u32, u32, String)>,
    analysis: &AnalyzedFile,
    range: TextRange,
    target: Url,
    tooltip: String,
) {
    if target == analysis.uri {
        return;
    }
    if target.scheme() == "file" {
        let exists = target
            .to_file_path()
            .ok()
            .map(|path| path.is_file())
            .unwrap_or(false);
        if !exists {
            return;
        }
    }
    let key = (range.start, range.end, target.to_string());
    if !seen.insert(key) {
        return;
    }
    links.push(DocumentLink {
        range: analysis.source.range(range),
        target: Some(target),
        tooltip: Some(tooltip),
        data: None,
    });
}

fn resolve_import_target(
    resolved: &ResolvedImport,
    workspace: &WorkspaceIndex,
    base: Option<&Path>,
) -> Option<Url> {
    match &resolved.import.path {
        ImportPath::File { path, .. } => {
            let path = PathBuf::from(path.as_str());
            let path = if path.is_absolute() {
                path
            } else {
                base?.join(path)
            };
            file_target(&path)
        }
        ImportPath::Roc { raw, segments, .. } => {
            let first = segments
                .first()
                .map(|name| name.text.as_str())
                .or_else(|| raw.split('.').next());
            if let Some(first) = first {
                if first
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_lowercase())
                    && workspace.package(first).is_some()
                {
                    let module_name = raw.split('.').skip(1).collect::<Vec<_>>().join("/");
                    if !module_name.is_empty() {
                        if let Some(module) = workspace.find_module(Some(first), &module_name) {
                            return Some(module.uri.clone());
                        }
                        if let Some(module) = workspace.find_module(
                            Some(first),
                            raw.split('.').next_back().unwrap_or(raw.as_str()),
                        ) {
                            return Some(module.uri.clone());
                        }
                    }
                    return package_root_target(first, workspace);
                }
            }
            let root_relative = raw.starts_with('/');
            let raw = raw.trim_start_matches('/');
            let base = if root_relative {
                workspace.root.clone()
            } else {
                base?.to_path_buf()
            };
            for relative in [format!("{raw}.roc"), format!("{raw}/main.roc")] {
                let candidate = base.join(relative);
                if candidate.is_file() {
                    return path_to_uri(&candidate);
                }
            }
            None
        }
    }
}

fn dependency_target(
    dependency: &Dependency,
    workspace: &WorkspaceIndex,
    base: Option<&Path>,
) -> Option<Url> {
    let direct = match &dependency.source {
        DependencySource::Url(url) => Url::parse(url).ok(),
        DependencySource::Path(path) => {
            let path = PathBuf::from(path.as_str());
            let path = if path.is_absolute() {
                path
            } else if let Some(base) = base {
                base.join(path)
            } else {
                path
            };
            file_target(&path)
        }
        DependencySource::RocVersion(_) | DependencySource::CompilerOwned(_) => None,
    };
    direct.or_else(|| package_root_target(&dependency.shorthand.text, workspace))
}

fn package_root_target(shorthand: &str, workspace: &WorkspaceIndex) -> Option<Url> {
    let package = workspace.package(shorthand)?;
    if let Some(root_file) = &package.root_file {
        return path_to_uri(root_file);
    }
    let root = package.root.as_deref()?;
    for name in ["main.roc", "app.roc", "package.roc"] {
        let candidate = root.join(name);
        if candidate.is_file() {
            return path_to_uri(&candidate);
        }
    }
    None
}

fn file_target(path: &Path) -> Option<Url> {
    if path.is_file() {
        return path_to_uri(path);
    }
    if path.is_dir() {
        for name in ["main.roc", "app.roc", "package.roc"] {
            let candidate = path.join(name);
            if candidate.is_file() {
                return path_to_uri(&candidate);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Code lenses
// ---------------------------------------------------------------------------

pub fn code_lenses(ctx: &FeatureContext<'_>, _params: &CodeLensParams) -> Vec<CodeLens> {
    let analysis = ctx.analysis;
    if in_package_cache(analysis) {
        return Vec::new();
    }
    let counts = reference_counts(ctx);
    let source = &*analysis.source;
    let mut lenses = Vec::new();
    let mut seen: HashSet<(u32, u32)> = HashSet::new();

    for symbol in &analysis.symbols {
        collect_definition_lenses(analysis, symbol, &counts, &mut lenses, &mut seen);
    }

    for item in &analysis.module().items {
        if let ItemKind::Expect(expect) = &item.kind {
            let line = source.line_of_offset(expect.range.start);
            let range = TextRange::new(source.line_start(line), source.line_end(line));
            if !seen.insert((range.start, range.end)) {
                continue;
            }
            let position = source.position_at(expect.range.start);
            let file = analysis
                .uri
                .to_file_path()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_else(|_| analysis.uri.to_string());
            lenses.push(CodeLens {
                range: source.range(range),
                command: Some(Command {
                    title: "Run test".to_string(),
                    command: "hardroc.runTest".to_string(),
                    arguments: Some(vec![
                        serde_json::json!(file),
                        serde_json::json!({
                            "line": position.line,
                            "character": position.character,
                        }),
                    ]),
                }),
                data: None,
            });
        }
    }

    lenses.sort_by_key(|lens| (lens.range.start.line, lens.range.start.character));
    lenses
}

fn collect_definition_lenses(
    analysis: &AnalyzedFile,
    symbol: &SymbolInfo,
    counts: &HashMap<DefinitionKey, u32>,
    lenses: &mut Vec<CodeLens>,
    seen: &mut HashSet<(u32, u32)>,
) {
    let is_top_level_definition = symbol.container.is_none()
        && matches!(&symbol.key, DefinitionKey::TopLevel { .. })
        && matches!(
            symbol.kind,
            SymbolKind::Function | SymbolKind::Constant | SymbolKind::Variable | SymbolKind::Method
        )
        && !symbol.name.starts_with('_')
        && !symbol.name_range.is_empty();
    if is_top_level_definition {
        let count = counts.get(&symbol.key).copied().unwrap_or(0);
        if seen.insert((symbol.name_range.start, symbol.name_range.end)) {
            let position = analysis.source.position_at(symbol.name_range.start);
            let title = if count == 1 {
                "1 reference".to_string()
            } else {
                format!("{count} references")
            };
            lenses.push(CodeLens {
                range: analysis.source.range(symbol.name_range),
                command: Some(Command {
                    title,
                    command: "hardroc.showReferences".to_string(),
                    arguments: Some(vec![
                        serde_json::json!(analysis.uri.as_str()),
                        serde_json::json!({
                            "line": position.line,
                            "character": position.character,
                        }),
                    ]),
                }),
                data: None,
            });
        }
    }
    for child in &symbol.children {
        collect_definition_lenses(analysis, child, counts, lenses, seen);
    }
}

fn reference_counts(ctx: &FeatureContext<'_>) -> HashMap<DefinitionKey, u32> {
    let mut counts = HashMap::new();
    let mut seen: HashSet<(Url, u32, u32)> = HashSet::new();
    add_references(ctx.analysis, &mut counts, &mut seen);
    for analysis in ctx.host.all_analyses() {
        if analysis.uri == ctx.analysis.uri {
            continue;
        }
        add_references(&analysis, &mut counts, &mut seen);
    }
    counts
}

fn add_references(
    analysis: &AnalyzedFile,
    counts: &mut HashMap<DefinitionKey, u32>,
    seen: &mut HashSet<(Url, u32, u32)>,
) {
    for reference in &analysis.references {
        let Some(key) = &reference.key else {
            continue;
        };
        if !matches!(key, DefinitionKey::TopLevel { .. }) {
            continue;
        }
        if seen.insert((
            analysis.uri.clone(),
            reference.range.start,
            reference.range.end,
        )) {
            *counts.entry(key.clone()).or_insert(0) += 1;
        }
    }
}

fn in_package_cache(analysis: &AnalyzedFile) -> bool {
    let Ok(path) = analysis.uri.to_file_path() else {
        return false;
    };
    roc_cache_dir()
        .map(|cache| path.starts_with(cache))
        .unwrap_or(false)
}

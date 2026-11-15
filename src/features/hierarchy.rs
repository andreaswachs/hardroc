//! Call hierarchy support.
//!
//! Roc functions are first-class, so a call hierarchy is inherently
//! conservative: direct calls and method calls are included, as are pipeline
//! applications (`value |> f`). Passing a function as a value is not treated as
//! a call.

use std::collections::HashMap;

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams, Range,
    SymbolKind,
};

use crate::analysis::{AnalyzedFile, DefinitionKey, SymbolInfo, SymbolKind as RocSymbolKind};
use crate::syntax::ast::{Expr, ExprKind, Stmt, StmtKind};
use crate::text::{SourceFile, TextRange};

use super::util::symbol_kind;
use super::FeatureContext;

pub fn prepare(
    ctx: &FeatureContext<'_>,
    params: &CallHierarchyPrepareParams,
) -> Option<Vec<CallHierarchyItem>> {
    let position = params.text_document_position_params.position;
    let offset = ctx.analysis.source.offset_at(position);
    let symbol = ctx.analysis.symbol_at(offset)?;
    if !is_callable(symbol) {
        return None;
    }
    Some(vec![item_for_symbol(ctx.analysis, symbol)])
}

pub fn incoming(
    ctx: &FeatureContext<'_>,
    params: &CallHierarchyIncomingCallsParams,
) -> Option<Vec<CallHierarchyIncomingCall>> {
    let target = key_from_item(ctx, &params.item)?;
    let mut grouped: HashMap<DefinitionKey, (CallHierarchyItem, Vec<Range>)> = HashMap::new();
    for analysis in ctx.host.all_analyses() {
        for reference in &analysis.references {
            if reference.key.as_ref() != Some(&target) {
                continue;
            }
            if !is_call_site(&analysis.source, reference.range) {
                continue;
            }
            let Some(caller) = enclosing_callable(&analysis, reference.range) else {
                continue;
            };
            if caller.key == target {
                continue;
            }
            let item = item_for_symbol(&analysis, caller);
            let entry = grouped
                .entry(caller.key.clone())
                .or_insert_with(|| (item, Vec::new()));
            let range = analysis.source.range(reference.range);
            if !entry.1.contains(&range) {
                entry.1.push(range);
            }
        }
    }
    let mut calls: Vec<_> = grouped
        .into_values()
        .map(|(from, from_ranges)| CallHierarchyIncomingCall { from, from_ranges })
        .collect();
    calls.sort_by(|a, b| {
        (
            &a.from.uri,
            a.from.selection_range.start.line,
            a.from.selection_range.start.character,
        )
            .cmp(&(
                &b.from.uri,
                b.from.selection_range.start.line,
                b.from.selection_range.start.character,
            ))
    });
    Some(calls)
}

pub fn outgoing(
    ctx: &FeatureContext<'_>,
    params: &CallHierarchyOutgoingCallsParams,
) -> Option<Vec<CallHierarchyOutgoingCall>> {
    let key = key_from_item(ctx, &params.item)?;
    let analysis = ctx.host.analyze(&params.item.uri)?;
    let symbol = analysis
        .symbols
        .iter()
        .flat_map(flatten_symbols)
        .find(|symbol| symbol.key == key)
        .cloned()
        .or_else(|| {
            analysis
                .symbols
                .iter()
                .flat_map(flatten_symbols)
                .find(|symbol| {
                    symbol.name == params.item.name
                        && symbol.name_range
                            == analysis.source.text_range(params.item.selection_range)
                })
                .cloned()
        })?;

    let mut sites = Vec::new();
    for definition in analysis.definitions() {
        if definition.range == symbol.full_range
            || definition.range.contains_inclusive(symbol.full_range.start)
        {
            collect_calls(&definition.value, &mut sites);
            for statement in definition.value.kind_block_statements() {
                collect_statement_calls(statement, &mut sites);
            }
        }
    }
    // Nested callables: include calls in the nearest enclosing definition.
    if sites.is_empty() {
        for definition in analysis.definitions() {
            if definition.range.contains_inclusive(symbol.full_range.start)
                && symbol.full_range.start >= definition.range.start
            {
                collect_calls(&definition.value, &mut sites);
            }
        }
    }

    let mut grouped: HashMap<DefinitionKey, (CallHierarchyItem, Vec<Range>)> = HashMap::new();
    for site in sites {
        let Some(target) = resolve_call_site(&analysis, &site) else {
            continue;
        };
        let item = item_for_key(ctx, &analysis, &target, site.range)
            .unwrap_or_else(|| fallback_item(&analysis, &site));
        let entry = grouped.entry(target).or_insert_with(|| (item, Vec::new()));
        let range = analysis.source.range(site.range);
        if !entry.1.contains(&range) {
            entry.1.push(range);
        }
    }
    let mut calls: Vec<_> = grouped
        .into_values()
        .map(|(to, from_ranges)| CallHierarchyOutgoingCall { to, from_ranges })
        .collect();
    calls.sort_by(|a, b| a.to.name.cmp(&b.to.name));
    Some(calls)
}

#[derive(Debug, Clone)]
struct CallSite {
    name: String,
    range: TextRange,
}

fn is_callable(symbol: &SymbolInfo) -> bool {
    matches!(
        symbol.kind,
        RocSymbolKind::Function | RocSymbolKind::Method | RocSymbolKind::Local
    )
}

fn flatten_symbols(symbol: &SymbolInfo) -> Vec<&SymbolInfo> {
    let mut output = vec![symbol];
    for child in &symbol.children {
        output.extend(flatten_symbols(child));
    }
    output
}

fn item_for_symbol(analysis: &AnalyzedFile, symbol: &SymbolInfo) -> CallHierarchyItem {
    CallHierarchyItem {
        name: symbol.name.clone(),
        kind: symbol_kind(symbol.kind),
        tags: None,
        detail: symbol.detail.clone().or_else(|| {
            analysis
                .signature_for(&symbol.key)
                .map(|s| s.display.clone())
        }),
        uri: analysis.uri.clone(),
        range: analysis.source.range(symbol.full_range),
        selection_range: analysis.source.range(symbol.name_range),
        data: serde_json::to_value(&symbol.key).ok(),
    }
}

fn key_from_item(ctx: &FeatureContext<'_>, item: &CallHierarchyItem) -> Option<DefinitionKey> {
    if let Some(data) = &item.data {
        if let Ok(key) = serde_json::from_value::<DefinitionKey>(data.clone()) {
            return Some(key);
        }
    }
    let analysis = ctx.host.analyze(&item.uri)?;
    let range = analysis.source.text_range(item.selection_range);
    analysis
        .symbols
        .iter()
        .flat_map(flatten_symbols)
        .find(|symbol| symbol.name_range == range && symbol.name == item.name)
        .map(|symbol| symbol.key.clone())
}

fn enclosing_callable(analysis: &AnalyzedFile, range: TextRange) -> Option<&SymbolInfo> {
    analysis
        .symbols
        .iter()
        .flat_map(flatten_symbols)
        .filter(|symbol| matches!(symbol.kind, RocSymbolKind::Function | RocSymbolKind::Method))
        .filter(|symbol| symbol.full_range.contains_inclusive(range.start))
        .min_by_key(|symbol| symbol.full_range.len())
}

fn item_for_key(
    ctx: &FeatureContext<'_>,
    analysis: &AnalyzedFile,
    key: &DefinitionKey,
    call_range: TextRange,
) -> Option<CallHierarchyItem> {
    match key {
        DefinitionKey::Builtin { full_name } => {
            let builtin = ctx.host.builtins().lookup(full_name)?;
            Some(CallHierarchyItem {
                name: builtin.short_name.clone(),
                kind: SymbolKind::FUNCTION,
                tags: None,
                detail: builtin.signature.clone(),
                uri: analysis.uri.clone(),
                range: analysis.source.range(call_range),
                selection_range: analysis.source.range(call_range),
                data: serde_json::to_value(key).ok(),
            })
        }
        DefinitionKey::Local { .. }
        | DefinitionKey::TopLevel { .. }
        | DefinitionKey::Module { .. } => {
            let (uri, range) = ctx.host.find_definition(key)?;
            let target = ctx.host.analyze(&uri)?;
            let symbol = target
                .symbols
                .iter()
                .flat_map(flatten_symbols)
                .find(|symbol| symbol.name_range == range)
                .or_else(|| {
                    target
                        .symbols
                        .iter()
                        .flat_map(flatten_symbols)
                        .find(|symbol| &symbol.key == key)
                });
            if let Some(symbol) = symbol {
                Some(item_for_symbol(&target, symbol))
            } else {
                Some(CallHierarchyItem {
                    name: key_name(key),
                    kind: SymbolKind::FUNCTION,
                    tags: None,
                    detail: None,
                    uri,
                    range: target.source.range(range),
                    selection_range: target.source.range(range),
                    data: serde_json::to_value(key).ok(),
                })
            }
        }
    }
}

fn fallback_item(analysis: &AnalyzedFile, site: &CallSite) -> CallHierarchyItem {
    CallHierarchyItem {
        name: site.name.clone(),
        kind: SymbolKind::FUNCTION,
        tags: None,
        detail: None,
        uri: analysis.uri.clone(),
        range: analysis.source.range(site.range),
        selection_range: analysis.source.range(site.range),
        data: None,
    }
}

fn key_name(key: &DefinitionKey) -> String {
    match key {
        DefinitionKey::Local { def_start, .. } => format!("local@{def_start}"),
        DefinitionKey::TopLevel { name, .. } => name.clone(),
        DefinitionKey::Builtin { full_name } => full_name.clone(),
        DefinitionKey::Module { .. } => "module".to_string(),
    }
}

fn is_call_site(source: &SourceFile, range: TextRange) -> bool {
    let text = source.text().as_bytes();
    let mut index = range.end as usize;
    while index < text.len() && matches!(text[index], b' ' | b'\t' | b'\r' | b'\n') {
        index += 1;
    }
    if text.get(index) == Some(&b'(') {
        return true;
    }
    let mut before = range.start as usize;
    while before > 0 && matches!(text[before - 1], b' ' | b'\t') {
        before -= 1;
    }
    before >= 2 && &text[before - 2..before] == b"|>"
}

fn collect_statement_calls(statement: &Stmt, sites: &mut Vec<CallSite>) {
    match &statement.kind {
        StmtKind::Definition(definition) => {
            collect_calls(&definition.value, sites);
            for statement in definition.value.kind_block_statements() {
                collect_statement_calls(statement, sites);
            }
        }
        StmtKind::Assignment { value, .. } => collect_calls(value, sites),
        StmtKind::TypeDecl(_) => {}
        StmtKind::Db(value) | StmtKind::Expect(value) => collect_calls(value, sites),
        StmtKind::Return(Some(value)) => collect_calls(value, sites),
        StmtKind::Crash(value) => collect_calls(value, sites),
        StmtKind::Expr(value) => collect_calls(value, sites),
        _ => {}
    }
}

fn collect_calls(expr: &Expr, sites: &mut Vec<CallSite>) {
    match &expr.kind {
        ExprKind::Call { callee, args } => {
            if let Some((name, range)) = callee_name(callee) {
                sites.push(CallSite { name, range });
            }
            collect_calls(callee, sites);
            for arg in args {
                collect_calls(arg, sites);
            }
        }
        ExprKind::MethodCall {
            receiver,
            method,
            args,
        } => {
            sites.push(CallSite {
                name: method.text.clone(),
                range: method.range,
            });
            collect_calls(receiver, sites);
            for arg in args {
                collect_calls(arg, sites);
            }
        }
        ExprKind::Binary { op, lhs, rhs, .. } => {
            if op == "|>" {
                if let Some((name, range)) = callee_name(rhs) {
                    sites.push(CallSite { name, range });
                }
            }
            collect_calls(lhs, sites);
            collect_calls(rhs, sites);
        }
        ExprKind::Tag { args, .. } => {
            for arg in args {
                collect_calls(arg, sites);
            }
        }
        ExprKind::List(items) => {
            for item in items {
                collect_calls(item, sites);
            }
        }
        ExprKind::Record { fields, update } => {
            for field in fields {
                if let Some(value) = &field.value {
                    collect_calls(value, sites);
                }
                if let Some(default) = &field.default {
                    collect_calls(default, sites);
                }
            }
            if let Some(update) = update {
                collect_calls(update, sites);
            }
        }
        ExprKind::Tuple(items) => {
            for item in items {
                collect_calls(item, sites);
            }
        }
        ExprKind::Lambda { body, .. } => {
            collect_calls(body, sites);
            for statement in body.kind_block_statements() {
                collect_statement_calls(statement, sites);
            }
        }
        ExprKind::Block { stmts, tail } => {
            for statement in stmts {
                collect_statement_calls(statement, sites);
            }
            if let Some(tail) = tail {
                collect_calls(tail, sites);
            }
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_calls(condition, sites);
            collect_calls(then_branch, sites);
            collect_calls(else_branch, sites);
        }
        ExprKind::Match { subject, branches } => {
            collect_calls(subject, sites);
            for branch in branches {
                if let Some(guard) = &branch.guard {
                    collect_calls(guard, sites);
                }
                collect_calls(&branch.body, sites);
            }
        }
        ExprKind::Field { base, .. } => collect_calls(base, sites),
        ExprKind::TupleIndex { base, .. } => collect_calls(base, sites),
        ExprKind::Unary { expr, .. } => collect_calls(expr, sites),
        ExprKind::Paren(inner) => collect_calls(inner, sites),
        ExprKind::For { iterable, body, .. } => {
            collect_calls(iterable, sites);
            collect_calls(body, sites);
        }
        ExprKind::While { condition, body } => {
            collect_calls(condition, sites);
            collect_calls(body, sites);
        }
        ExprKind::Return(Some(value)) => collect_calls(value, sites),
        ExprKind::Crash(value) => collect_calls(value, sites),
        ExprKind::Db(value) => collect_calls(value, sites),
        ExprKind::Nominal { value, .. } => collect_calls(value, sites),
        ExprKind::String(parts) => {
            for part in parts {
                if let crate::syntax::ast::StrPartKind::Interpolation(expr) = &part.kind {
                    collect_calls(expr, sites);
                }
            }
        }
        _ => {}
    }
}

fn callee_name(expr: &Expr) -> Option<(String, TextRange)> {
    match &expr.kind {
        ExprKind::Lookup { path } => path.last().map(|name| (name.text.clone(), name.range)),
        ExprKind::Field { field, .. } => Some((field.text.clone(), field.range)),
        ExprKind::Paren(inner) => callee_name(inner),
        _ => None,
    }
}

fn resolve_call_site(analysis: &AnalyzedFile, site: &CallSite) -> Option<DefinitionKey> {
    if let Some(reference) = analysis.reference_at(site.range.start) {
        if let Some(key) = &reference.key {
            return Some(key.clone());
        }
    }
    // Builtin calls may not produce references if analysis was conservative.
    let text = analysis.source.slice(site.range);
    if let Some(item) = analysis.builtins_lookup(text) {
        return Some(item.key());
    }
    None
}

trait ExprBlockStatements {
    fn kind_block_statements(&self) -> &[Stmt];
}

impl ExprBlockStatements for Expr {
    fn kind_block_statements(&self) -> &[Stmt] {
        match &self.kind {
            ExprKind::Block { stmts, .. } => stmts,
            _ => &[],
        }
    }
}

trait AnalysisBuiltins {
    fn builtins_lookup(&self, name: &str) -> Option<&crate::analysis::BuiltinItem>;
}

impl AnalysisBuiltins for AnalyzedFile {
    fn builtins_lookup(&self, name: &str) -> Option<&crate::analysis::BuiltinItem> {
        // Builtin calls in a workspace are always resolved through references
        // when analysis knows the full path; the fallback intentionally stays
        // empty rather than guessing an ambiguous short name.
        let _ = name;
        None
    }
}

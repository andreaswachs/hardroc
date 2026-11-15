//! Completion, hover, signature help, and inlay hints.
//!
//! The features here are driven by the analysis layer: symbols, references,
//! scopes, signatures, resolved imports, and the generated builtin API.  Syntax
//! tokens supply the *editor* context (what is being typed right now), because
//! completion and signature help must work on partially written code.

use std::collections::HashSet;

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse, CompletionTextEdit,
    Documentation, Hover, HoverContents, HoverParams, InlayHint, InlayHintKind, InlayHintLabel,
    InlayHintParams, InlayHintTooltip, InsertTextFormat, ParameterInformation, ParameterLabel,
    SignatureHelp, SignatureHelpParams, SignatureInformation, TextEdit, Url,
};
use serde_json::{json, Value};

use super::{util, FeatureContext};
use crate::analysis::index::Export;
use crate::analysis::packages::{parse_url_version, resolve_dependency_root, uri_for_path};
use crate::analysis::{
    AnalyzedFile, BuiltinIndex, BuiltinItem, BuiltinKind, DefinitionKey, ModuleSummary,
    PackageInfo, ParamInfo, Signature, SymbolInfo, SymbolKind,
};
use crate::syntax::ast::{
    Definition, Expr, ExprKind, ImportPath, Item, ItemKind, Pattern, PatternKind, Stmt, StmtKind,
    TypeDecl, TypeDeclKind, TypeExpr, TypeField, TypeKind, TypeTag,
};
use crate::syntax::token::{Token, TokenKind};
use crate::text::{SourceFile, TextRange};

// ---------------------------------------------------------------------------
// Small token-context helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct SigToken<'a> {
    token: &'a Token,
    text: &'a str,
}

impl<'a> SigToken<'a> {
    fn kind(self) -> TokenKind {
        self.token.kind
    }

    fn start(self) -> u32 {
        self.token.range.start
    }

    fn end(self) -> u32 {
        self.token.range.end
    }

    fn is_name(self) -> bool {
        matches!(
            self.token.kind,
            TokenKind::LowerIdent | TokenKind::UpperIdent
        )
    }
}

fn significant<'a>(analysis: &'a AnalyzedFile) -> Vec<SigToken<'a>> {
    analysis
        .parsed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .map(|token| SigToken {
            token,
            text: token.text(analysis.source.text()),
        })
        .collect()
}

/// Whether the token sequence before a dot is an arbitrary expression rather
/// than a plain dotted name.  `Widget.make("x").label.` and
/// `Dict.empty().insert(...).` end their receiver in `)`/`]`/`}`, while
/// `Widget.label.` and `pkg.Mod.member.` are handled by `chain_before`.
fn chain_has_expression_receiver(sig: &[SigToken<'_>], dot_index: usize) -> bool {
    let mut name_index = dot_index;
    if name_index >= 1 && sig[name_index - 1].is_name() {
        name_index -= 1;
    } else {
        return name_index >= 1 && expression_end_token(sig[name_index - 1]);
    }
    name_index >= 2
        && sig[name_index - 1].kind() == TokenKind::Dot
        && expression_end_token(sig[name_index - 2])
}

fn expression_end_token(token: SigToken<'_>) -> bool {
    token.kind().is_literal()
        || matches!(
            token.kind(),
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace
        )
}

fn chain_before(sig: &[SigToken<'_>], dot_index: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut index = dot_index;
    while index >= 1 {
        let name = sig[index - 1];
        if !name.is_name() {
            break;
        }
        parts.push(name.text.to_string());
        if index >= 2 && sig[index - 2].kind() == TokenKind::Dot {
            index -= 2;
        } else {
            break;
        }
    }
    parts.reverse();
    parts
}

/// Find the expression immediately before a dot at `dot_start`.
///
/// Method/member completion normally gets a plain dotted name from
/// `chain_before`, but a receiver can be any expression:
/// `Dict.empty().in`, `Widget.make("x").`, or `row.items.`.  The parser leaves
/// the trailing `.name` as an error while the user is typing, so the receiver
/// is simply the innermost expression ending at (or just before) the dot.
fn expr_before_dot<'a>(ctx: &FeatureContext<'a>, dot_start: u32) -> Option<&'a Expr> {
    let source = ctx.analysis.source.as_ref();
    let mut search = ExprBeforeDot {
        source,
        dot_start,
        // A chained call may put the dot at the start of the next line:
        // `Dict.empty()\n    .insert(...)`.  Looking back one line keeps
        // unrelated expressions from earlier lines out of consideration.
        line_start: source.line_start(source.line_of_offset(dot_start).saturating_sub(1)),
        best: None,
        definitions: Vec::new(),
    };
    search.visit_items(&ctx.analysis.parsed.module.items);
    search.best
}

/// Find a local definition by its symbol range.  `all_definitions` only walks
/// top-level items and type members, while locals live inside blocks/lambdas.
fn local_definition<'a>(
    analysis: &'a AnalyzedFile,
    name: &str,
    name_range: TextRange,
    full_range: TextRange,
) -> Option<&'a Definition> {
    let source = analysis.source.as_ref();
    let mut search = ExprBeforeDot {
        source,
        dot_start: 0,
        line_start: 0,
        best: None,
        definitions: Vec::new(),
    };
    search.visit_items(&analysis.parsed.module.items);
    search.definitions.into_iter().find(|definition| {
        definition
            .simple_name()
            .is_some_and(|candidate| candidate.text == name && candidate.range == name_range)
            && definition.range == full_range
    })
}

fn all_definitions_deep(analysis: &AnalyzedFile) -> Vec<&Definition> {
    let mut search = ExprBeforeDot {
        source: analysis.source.as_ref(),
        dot_start: 0,
        line_start: 0,
        best: None,
        definitions: Vec::new(),
    };
    search.visit_items(&analysis.parsed.module.items);
    search.definitions
}

struct ExprBeforeDot<'a> {
    source: &'a SourceFile,
    dot_start: u32,
    line_start: u32,
    best: Option<&'a Expr>,
    definitions: Vec<&'a Definition>,
}

impl<'a> ExprBeforeDot<'a> {
    fn visit_items(&mut self, items: &'a [Item]) {
        for item in items {
            match &item.kind {
                ItemKind::Definition(definition) => {
                    self.definitions.push(definition);
                    self.visit_expr(&definition.value);
                }
                ItemKind::TypeDecl(decl) => self.visit_type_decl(decl),
                ItemKind::Expect(expect) => self.visit_expr(&expect.expr),
                ItemKind::Annotation(_) | ItemKind::WhereAlias(_) | ItemKind::Error => {}
            }
        }
    }

    fn visit_type_decl(&mut self, decl: &'a TypeDecl) {
        self.visit_type_expr(&decl.backing);
        self.visit_items(&decl.members);
    }

    fn visit_type_expr(&mut self, ty: &'a TypeExpr) {
        match &ty.kind {
            TypeKind::Function { args, ret, .. } => {
                for arg in args {
                    self.visit_type_expr(arg);
                }
                self.visit_type_expr(ret);
            }
            TypeKind::Tuple(items) => {
                for item in items {
                    self.visit_type_expr(item);
                }
            }
            TypeKind::Record { fields, .. } => {
                for field in fields {
                    self.visit_type_expr(&field.ty);
                    if let Some(default) = &field.default {
                        self.visit_expr(default);
                    }
                }
            }
            TypeKind::TagUnion { tags, .. } => {
                for tag in tags {
                    for payload in &tag.payloads {
                        self.visit_type_expr(payload);
                    }
                }
            }
            TypeKind::Apply { args, .. } => {
                for arg in args {
                    self.visit_type_expr(arg);
                }
            }
            TypeKind::Where { base, .. } => self.visit_type_expr(base),
            TypeKind::Named(_) | TypeKind::Var(_) | TypeKind::Wildcard | TypeKind::Error => {}
        }
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match &stmt.kind {
            StmtKind::Definition(definition) => {
                self.definitions.push(definition);
                self.visit_expr(&definition.value);
            }
            StmtKind::TypeDecl(decl) => self.visit_type_decl(decl),
            StmtKind::Assignment { value, .. }
            | StmtKind::Db(value)
            | StmtKind::Expect(value)
            | StmtKind::Crash(value)
            | StmtKind::Expr(value) => self.visit_expr(value),
            StmtKind::Return(Some(value)) => self.visit_expr(value),
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        let range = expr.range;
        if range.end <= self.dot_start
            && range.end >= self.line_start
            && !matches!(expr.kind, ExprKind::Error)
        {
            let between = self.source.slice(TextRange::new(range.end, self.dot_start));
            if between.trim().is_empty() && between.matches('\n').count() <= 1 {
                let replace = self.best.map_or(true, |best| {
                    range.end > best.range.end
                        || (range.end == best.range.end && range.start >= best.range.start)
                });
                if replace {
                    self.best = Some(expr);
                }
            }
        }

        match &expr.kind {
            ExprKind::Tag { args, .. } | ExprKind::List(args) | ExprKind::Tuple(args) => {
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            ExprKind::Record { fields, update } => {
                for field in fields {
                    if let Some(value) = &field.value {
                        self.visit_expr(value);
                    }
                    if let Some(default) = &field.default {
                        self.visit_expr(default);
                    }
                }
                if let Some(update) = update {
                    self.visit_expr(update);
                }
            }
            ExprKind::Lambda { body, .. } => self.visit_expr(body),
            ExprKind::Block { stmts, tail } => {
                for stmt in stmts {
                    self.visit_stmt(stmt);
                }
                if let Some(tail) = tail {
                    self.visit_expr(tail);
                }
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.visit_expr(condition);
                self.visit_expr(then_branch);
                self.visit_expr(else_branch);
            }
            ExprKind::Match { subject, branches } => {
                self.visit_expr(subject);
                for branch in branches {
                    if let Some(guard) = &branch.guard {
                        self.visit_expr(guard);
                    }
                    self.visit_expr(&branch.body);
                }
            }
            ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => {
                self.visit_expr(base);
            }
            ExprKind::Call { callee, args }
            | ExprKind::MethodCall {
                receiver: callee,
                args,
                ..
            } => {
                self.visit_expr(callee);
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.visit_expr(lhs);
                self.visit_expr(rhs);
            }
            ExprKind::Unary { expr, .. }
            | ExprKind::Paren(expr)
            | ExprKind::Return(Some(expr))
            | ExprKind::Crash(expr)
            | ExprKind::Db(expr) => self.visit_expr(expr),
            ExprKind::For { iterable, body, .. } => {
                self.visit_expr(iterable);
                self.visit_expr(body);
            }
            ExprKind::While { condition, body } => {
                self.visit_expr(condition);
                self.visit_expr(body);
            }
            ExprKind::Nominal { value, .. } => self.visit_expr(value),
            ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::MultilineString(_)
            | ExprKind::Char(_)
            | ExprKind::Lookup { .. }
            | ExprKind::Return(None)
            | ExprKind::Break
            | ExprKind::Continue
            | ExprKind::Error => {}
        }
    }
}

fn open_bracket_before(sig: &[SigToken<'_>], offset: u32) -> Option<(usize, TokenKind)> {
    let mut stack: Vec<(usize, TokenKind)> = Vec::new();
    for (index, token) in sig.iter().enumerate() {
        if token.start() >= offset {
            break;
        }
        match token.kind() {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => {
                stack.push((index, token.kind()));
            }
            TokenKind::RParen => pop_kind(&mut stack, TokenKind::LParen),
            TokenKind::RBrace => pop_kind(&mut stack, TokenKind::LBrace),
            TokenKind::RBracket => pop_kind(&mut stack, TokenKind::LBracket),
            _ => {}
        }
    }
    stack.pop()
}

fn pop_kind(stack: &mut Vec<(usize, TokenKind)>, kind: TokenKind) {
    if stack.last().is_some_and(|(_, open)| *open == kind) {
        stack.pop();
    }
}

fn top_level_commas(sig: &[SigToken<'_>], open_index: usize, offset: u32) -> u32 {
    let mut depth = 0u32;
    let mut commas = 0u32;
    for token in sig.iter().skip(open_index + 1) {
        if token.start() >= offset {
            break;
        }
        match token.kind() {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                depth = depth.saturating_sub(1)
            }
            TokenKind::Comma if depth == 0 => commas += 1,
            _ => {}
        }
    }
    commas
}

fn is_pipe(token: SigToken<'_>) -> bool {
    token.text == "|>"
}

fn is_expression_end(token: SigToken<'_>) -> bool {
    token.is_name()
        || token.kind().is_literal()
        || matches!(
            token.kind(),
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace
        )
}

fn previous_token<'a>(sig: &[SigToken<'a>], index: usize) -> Option<SigToken<'a>> {
    if index == 0 {
        None
    } else {
        Some(sig[index - 1])
    }
}

fn token_index_at(sig: &[SigToken<'_>], offset: u32) -> Option<usize> {
    sig.iter()
        .position(|token| token.token.range.contains(offset))
        .or_else(|| {
            sig.iter()
                .position(|token| token.token.range.contains_inclusive(offset))
        })
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'!' || byte == b'$'
}

fn prefix_at(source: &SourceFile, offset: u32) -> (String, TextRange) {
    let text = source.text().as_bytes();
    let offset = (offset as usize).min(text.len());
    let mut start = offset;
    while start > 0 && is_identifier_byte(text[start - 1]) {
        start -= 1;
    }
    let mut end = offset;
    while end < text.len() && is_identifier_byte(text[end]) {
        end += 1;
    }
    (
        source.text()[start..offset].to_string(),
        TextRange::new(start as u32, end as u32),
    )
}

// ---------------------------------------------------------------------------
// Completion items
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Spec {
    label: String,
    insert: Option<String>,
    kind: CompletionItemKind,
    detail: Option<String>,
    doc: Option<String>,
    filter: Option<String>,
    snippet: bool,
    group: u8,
    data: Option<Value>,
}

impl Spec {
    fn new(label: impl Into<String>, kind: CompletionItemKind, group: u8) -> Self {
        Self {
            label: label.into(),
            insert: None,
            kind,
            detail: None,
            doc: None,
            filter: None,
            snippet: false,
            group,
            data: None,
        }
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    fn doc(mut self, doc: impl Into<String>) -> Self {
        let doc = doc.into();
        if !doc.trim().is_empty() {
            self.doc = Some(doc);
        }
        self
    }

    fn insert(mut self, insert: impl Into<String>) -> Self {
        self.insert = Some(insert.into());
        self
    }

    fn data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    fn snippet(mut self) -> Self {
        self.snippet = true;
        self
    }
}

fn push_spec(out: &mut Vec<Spec>, seen: &mut HashSet<String>, spec: Spec) {
    if !seen.insert(spec.label.clone()) {
        return;
    }
    out.push(spec);
}

fn build_items(
    specs: Vec<Spec>,
    source: &SourceFile,
    replace: TextRange,
    resolve_documentation: bool,
) -> Vec<CompletionItem> {
    specs
        .into_iter()
        .map(|spec| {
            let new_text = spec.insert.clone().unwrap_or_else(|| spec.label.clone());
            CompletionItem {
                label: spec.label.clone(),
                label_details: None,
                kind: Some(spec.kind),
                detail: spec.detail.clone(),
                documentation: if resolve_documentation {
                    spec.doc
                        .clone()
                        .map(|doc| Documentation::MarkupContent(util::markdown(doc)))
                } else {
                    None
                },
                deprecated: None,
                preselect: None,
                sort_text: Some(format!("{}_{}", spec.group, spec.label.to_lowercase())),
                filter_text: Some(spec.filter.clone().unwrap_or_else(|| spec.label.clone())),
                insert_text: None,
                insert_text_format: if spec.snippet {
                    Some(InsertTextFormat::SNIPPET)
                } else {
                    None
                },
                insert_text_mode: None,
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: source.range(replace),
                    new_text,
                })),
                additional_text_edits: None,
                command: None,
                commit_characters: None,
                data: spec.data,
                tags: None,
            }
        })
        .collect()
}

fn completion_kind(kind: SymbolKind) -> CompletionItemKind {
    match kind {
        SymbolKind::Module => CompletionItemKind::MODULE,
        SymbolKind::Package => CompletionItemKind::MODULE,
        SymbolKind::Type => CompletionItemKind::STRUCT,
        SymbolKind::Function => CompletionItemKind::FUNCTION,
        SymbolKind::Method => CompletionItemKind::METHOD,
        SymbolKind::Constant => CompletionItemKind::CONSTANT,
        SymbolKind::Variable => CompletionItemKind::VARIABLE,
        SymbolKind::Field => CompletionItemKind::FIELD,
        SymbolKind::Tag => CompletionItemKind::ENUM_MEMBER,
        SymbolKind::TypeParameter => CompletionItemKind::TYPE_PARAMETER,
        SymbolKind::Parameter | SymbolKind::Local => CompletionItemKind::VARIABLE,
        SymbolKind::Import => CompletionItemKind::MODULE,
    }
}

fn kind_label(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Module => "module",
        SymbolKind::Package => "package",
        SymbolKind::Type => "type",
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Constant => "constant",
        SymbolKind::Variable => "variable",
        SymbolKind::Field => "field",
        SymbolKind::Tag => "tag",
        SymbolKind::TypeParameter => "type parameter",
        SymbolKind::Parameter => "parameter",
        SymbolKind::Local => "local",
        SymbolKind::Import => "import",
    }
}

// ---------------------------------------------------------------------------
// Completion entry point
// ---------------------------------------------------------------------------

pub fn completion(
    ctx: &FeatureContext<'_>,
    params: &CompletionParams,
) -> Option<CompletionResponse> {
    let position = params.text_document_position.position;
    let offset = util::offset_at(ctx.analysis, position);
    let source = ctx.analysis.source.as_ref();

    if let Some(token) = ctx.analysis.token_at(offset) {
        if token.kind.is_comment() {
            return None;
        }
        if matches!(
            token.kind,
            TokenKind::String | TokenKind::MultilineString | TokenKind::Char
        ) {
            return None;
        }
    }

    let (prefix, replace) = prefix_at(source, offset);
    let sig = significant(ctx.analysis);
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    if import_completion(ctx, &sig, offset, &prefix, replace, &mut out, &mut seen) {
        return Some(CompletionResponse::Array(build_items(
            out,
            source,
            replace,
            ctx.config.completion.resolve_documentation,
        )));
    }

    if header_completion(ctx, offset, &mut out, &mut seen) {
        return Some(CompletionResponse::Array(build_items(
            out,
            source,
            replace,
            ctx.config.completion.resolve_documentation,
        )));
    }

    let prefix_start = replace.start;
    if let Some(index) = token_index_at(&sig, prefix_start.saturating_sub(1)) {
        if sig[index].kind() == TokenKind::Dot {
            let parts = if chain_has_expression_receiver(&sig, index) {
                Vec::new()
            } else {
                chain_before(&sig, index)
            };
            if !parts.is_empty() {
                complete_qualified(ctx, &parts, offset, &mut out, &mut seen);
                if !out.is_empty() {
                    return Some(CompletionResponse::Array(build_items(
                        out,
                        source,
                        replace,
                        ctx.config.completion.resolve_documentation,
                    )));
                }
            }
            // A chain whose receiver is not a plain name: `Dict.empty().in`,
            // `Widget.make("x").`, `settings.greet.`.  Those parse as an
            // arbitrary expression followed by a dot, so resolve the type of
            // the expression ending at the dot and offer its members.
            if let Some(base) = expr_before_dot(ctx, sig[index].start()) {
                if let Some(type_ref) = infer_expr_type(ctx, base, 8) {
                    if let Some(target) = resolve_type_ref(ctx, &type_ref, &[]) {
                        push_target(ctx, target, &mut out, &mut seen);
                        if !out.is_empty() {
                            return Some(CompletionResponse::Array(build_items(
                                out,
                                source,
                                replace,
                                ctx.config.completion.resolve_documentation,
                            )));
                        }
                    }
                }
            }
        }
    }

    if record_field_completion(ctx, &sig, offset, &mut out, &mut seen) {
        return Some(CompletionResponse::Array(build_items(
            out,
            source,
            replace,
            ctx.config.completion.resolve_documentation,
        )));
    }

    if tag_completion(ctx, &sig, offset, &mut out, &mut seen) {
        return Some(CompletionResponse::Array(build_items(
            out,
            source,
            replace,
            ctx.config.completion.resolve_documentation,
        )));
    }

    general_completion(ctx, offset, &mut out, &mut seen);
    Some(CompletionResponse::Array(build_items(
        out,
        source,
        replace,
        ctx.config.completion.resolve_documentation,
    )))
}

pub fn resolve_completion(ctx: &FeatureContext<'_>, mut item: CompletionItem) -> CompletionItem {
    let Some(data) = item.data.clone() else {
        return item;
    };
    let kind = data.get("kind").and_then(Value::as_str).unwrap_or("");
    match kind {
        "builtin" => {
            if let Some(name) = data.get("name").and_then(Value::as_str) {
                if let Some(builtin) = ctx.host.builtins().lookup(name) {
                    if item.detail.is_none() {
                        item.detail = builtin
                            .signature
                            .clone()
                            .or_else(|| builtin.type_def.clone())
                            .or_else(|| Some(builtin.kind_string().to_string()));
                    }
                    if item.documentation.is_none() {
                        if let Some(doc) = builtin.doc.clone() {
                            item.documentation =
                                Some(Documentation::MarkupContent(util::markdown(doc)));
                        }
                    }
                }
            }
        }
        "symbol" => {
            let uri = data
                .get("uri")
                .and_then(Value::as_str)
                .and_then(|value| Url::parse(value).ok());
            let name = data.get("name").and_then(Value::as_str).unwrap_or("");
            let container = data.get("container").and_then(Value::as_str);
            if let Some(uri) = uri {
                if let Some(analysis) = ctx.host.analyze(&uri) {
                    if let Some(symbol) = find_symbol(&analysis, name, container) {
                        if item.detail.is_none() {
                            item.detail = symbol.detail.clone();
                        }
                        if item.documentation.is_none() {
                            if let Some(doc) = symbol.doc.clone() {
                                item.documentation =
                                    Some(Documentation::MarkupContent(util::markdown(doc)));
                            }
                        }
                    }
                }
            }
        }
        "module" | "package" if item.documentation.is_none() => {
            let name = data.get("name").and_then(Value::as_str).unwrap_or("");
            let module = ctx
                .host
                .workspace()
                .modules
                .iter()
                .find(|module| module.name == name)
                .cloned();
            if let Some(doc) = module.and_then(|module| module.doc) {
                item.documentation = Some(Documentation::MarkupContent(util::markdown(doc)));
            } else {
                let detail = format!("module `{name}`");
                item.documentation = Some(Documentation::MarkupContent(util::markdown(detail)));
            }
        }
        "field" | "tag" if item.documentation.is_none() => {
            let name = data.get("name").and_then(Value::as_str).unwrap_or("");
            if let Some(symbol) = find_symbol(ctx.analysis, name, None) {
                if let Some(doc) = symbol.doc.clone() {
                    item.documentation = Some(Documentation::MarkupContent(util::markdown(doc)));
                }
            }
        }
        _ => {}
    }
    item
}

// ---------------------------------------------------------------------------
// Import completion
// ---------------------------------------------------------------------------

fn import_completion(
    ctx: &FeatureContext<'_>,
    sig: &[SigToken<'_>],
    offset: u32,
    prefix: &str,
    replace: TextRange,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) -> bool {
    let Some(import_index) = find_import_keyword(ctx, sig, offset) else {
        return false;
    };
    let exposing_index = sig
        .iter()
        .enumerate()
        .skip(import_index + 1)
        .find(|(_index, token)| {
            token.start() < offset && token.token.is_keyword(crate::syntax::Keyword::Exposing)
        })
        .map(|(index, _)| index);

    if let Some(index) = exposing_index {
        if sig[index].end() <= offset {
            let path_text = import_path_text(ctx, import_index, index);
            complete_exposing(ctx, &path_text, out, seen);
            return true;
        }
    }

    // Complete only the current path segment, but keep the prefix already
    // typed before it so package-qualified modules are inserted once.
    let (_, segment) = import_segment(ctx, import_index, offset);
    let raw_path = ctx.analysis.source.slice(TextRange::new(
        significant(ctx.analysis)[import_index].end(),
        offset,
    ));
    let raw_path = raw_path.trim_start();
    let before = raw_path
        .rfind(['.', '/'])
        .map(|index| &raw_path[..index])
        .unwrap_or("");
    let workspace = ctx.host.workspace();
    let package = before.split('.').next().unwrap_or("");
    if !package.is_empty()
        && (workspace.package(package).is_some() || header_dependency(ctx, package).is_some())
    {
        let module_prefix = before
            .strip_prefix(package)
            .unwrap_or("")
            .trim_start_matches('.');
        complete_import_in_package(ctx, package, module_prefix, out, seen);
    } else {
        complete_import_root(ctx, &segment, out, seen);
    }
    let _ = replace;
    let _ = prefix;
    true
}

fn find_import_keyword(
    ctx: &FeatureContext<'_>,
    sig: &[SigToken<'_>],
    offset: u32,
) -> Option<usize> {
    if let Some(resolved) = ctx
        .analysis
        .imports
        .iter()
        .find(|resolved| resolved.import.range.contains_inclusive(offset))
    {
        if let Some(index) = sig.iter().position(|token| {
            token.token.is_keyword(crate::syntax::Keyword::Import)
                && resolved.import.range.contains_inclusive(token.start())
        }) {
            return Some(index);
        }
    }

    let line_start = ctx
        .analysis
        .source
        .line_start(ctx.analysis.source.line_of_offset(offset));
    let mut candidate = None;
    for (index, token) in sig.iter().enumerate() {
        if token.start() > offset {
            break;
        }
        if token.token.is_keyword(crate::syntax::Keyword::Import) {
            candidate = Some(index);
        }
    }
    let index = candidate?;
    if sig[index].start() >= line_start {
        return Some(index);
    }
    // Multi-line imports: accept when the cursor is inside brackets opened by
    // the import statement.
    let mut depth = 0i32;
    for token in sig.iter().skip(index + 1) {
        if token.start() >= offset {
            break;
        }
        match token.kind() {
            TokenKind::LBracket | TokenKind::LParen | TokenKind::LBrace => depth += 1,
            TokenKind::RBracket | TokenKind::RParen | TokenKind::RBrace => {
                depth = (depth - 1).max(0)
            }
            TokenKind::Eq => return None,
            _ => {}
        }
    }
    if depth > 0 {
        Some(index)
    } else {
        None
    }
}

fn import_path_text(ctx: &FeatureContext<'_>, import_index: usize, end_index: usize) -> String {
    let sig = significant(ctx.analysis);
    let start = sig[import_index].end();
    let end = sig[end_index].start();
    if start >= end {
        return String::new();
    }
    ctx.analysis
        .source
        .slice(TextRange::new(start, end))
        .trim()
        .trim_end_matches(|ch: char| ch == '[' || ch.is_whitespace())
        .to_string()
}

fn import_segment(ctx: &FeatureContext<'_>, import_index: usize, offset: u32) -> (u32, String) {
    let sig = significant(ctx.analysis);
    let start = sig[import_index].end();
    let text = ctx
        .analysis
        .source
        .slice(TextRange::new(start, offset))
        .trim_start();
    let prefix_offset = start
        + (ctx
            .analysis
            .source
            .slice(TextRange::new(start, offset))
            .len()
            - ctx
                .analysis
                .source
                .slice(TextRange::new(start, offset))
                .trim_start()
                .len()) as u32;
    let trimmed = text;
    let separator = trimmed
        .rfind(['.', '/'])
        .map(|index| index + 1)
        .unwrap_or(0);
    let segment = trimmed[separator..].to_string();
    (prefix_offset + separator as u32, segment)
}

fn complete_import_root(
    ctx: &FeatureContext<'_>,
    prefix: &str,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) {
    let workspace = ctx.host.workspace();
    let mut shorthands = Vec::new();
    if let Some(header) = &ctx.analysis.parsed.module.header {
        for dependency in &header.dependencies {
            shorthands.push((
                dependency.shorthand.text.clone(),
                dependency.source.as_str().to_string(),
                dependency.is_platform,
            ));
        }
    }
    for package in &workspace.packages {
        if !shorthands
            .iter()
            .any(|(name, _, _)| *name == package.shorthand)
        {
            shorthands.push((
                package.shorthand.clone(),
                package.source_display(),
                matches!(package.kind, crate::analysis::PackageKind::Platform),
            ));
        }
    }
    for (name, source, platform) in shorthands {
        let kind = if platform { "platform" } else { "package" };
        push_spec(
            out,
            seen,
            Spec::new(name.clone(), CompletionItemKind::MODULE, 2)
                .detail(format!("{kind} · {source}"))
                .insert(format!("{name}."))
                .data(json!({"kind": "package", "name": name})),
        );
        if ctx.config.completion.package_members {
            let modules = package_module_names(ctx, &name);
            for module in modules {
                push_spec(
                    out,
                    seen,
                    Spec::new(format!("{name}.{module}"), CompletionItemKind::MODULE, 2)
                        .detail(format!("{name} module `{module}`"))
                        .data(json!({"kind": "module", "name": module})),
                );
            }
        }
    }

    for module in &workspace.modules {
        if module.package.is_some() || workspace.is_package_root(&module.path) {
            continue;
        }
        let name = module.name.clone();
        if name.contains('/') {
            push_spec(
                out,
                seen,
                Spec::new(name.clone(), CompletionItemKind::MODULE, 2)
                    .detail(format!(
                        "{} · {}",
                        module.kind.as_str(),
                        module.path.display()
                    ))
                    .doc(module.doc.clone().unwrap_or_default())
                    .data(json!({"kind": "module", "name": name})),
            );
        } else {
            push_spec(
                out,
                seen,
                Spec::new(name.clone(), CompletionItemKind::MODULE, 2)
                    .detail(module.kind.as_str())
                    .doc(module.doc.clone().unwrap_or_default())
                    .data(json!({"kind": "module", "name": name})),
            );
        }
        // Offer the modules in a subdirectory of this module path.
        let prefix = format!("{}/", module.name);
        for candidate in &workspace.modules {
            if candidate.package.is_none() {
                if let Some(tail) = candidate.name.strip_prefix(&prefix) {
                    push_spec(
                        out,
                        seen,
                        Spec::new(
                            format!("{}/{}", module.name, tail),
                            CompletionItemKind::MODULE,
                            2,
                        )
                        .detail(format!(
                            "{} · {}",
                            candidate.kind.as_str(),
                            candidate.path.display()
                        ))
                        .data(json!({"kind": "module", "name": candidate.name})),
                    );
                }
            }
        }
    }

    let module_name = &ctx.analysis.module_name;
    push_spec(
        out,
        seen,
        Spec::new(module_name.clone(), CompletionItemKind::MODULE, 2)
            .detail("this module")
            .insert(module_name.clone()),
    );

    let _ = prefix;
}

fn complete_import_in_package(
    ctx: &FeatureContext<'_>,
    package: &str,
    module_prefix: &str,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) {
    for module in package_module_names(ctx, package) {
        let suffix = if module_prefix.is_empty() {
            module.clone()
        } else if module == module_prefix {
            continue;
        } else if let Some(suffix) = module.strip_prefix(&format!("{module_prefix}.")) {
            suffix.to_string()
        } else {
            continue;
        };
        let mut spec = Spec::new(suffix.clone(), CompletionItemKind::MODULE, 2)
            .detail(format!("{package} module `{module}`"))
            .data(json!({"kind": "module", "name": module}));
        spec.filter = Some(suffix);
        push_spec(out, seen, spec);
    }
}

fn package_module_names(ctx: &FeatureContext<'_>, package: &str) -> Vec<String> {
    let mut names = Vec::new();
    let workspace = ctx.host.workspace();
    let roots: Vec<_> = workspace
        .packages
        .iter()
        .filter(|info| info.has_alias(package))
        .filter_map(|info| info.root.as_deref())
        .collect();
    for module in &workspace.modules {
        let belongs = module.package.as_deref() == Some(package)
            || module
                .package_root
                .as_deref()
                .is_some_and(|root| roots.contains(&root));
        if belongs {
            let Some(public) = workspace.public_name_for_path(&module.path) else {
                continue;
            };
            let name = public.replace('/', ".");
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

fn complete_exposing(
    ctx: &FeatureContext<'_>,
    path_text: &str,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) {
    let path_text = path_text.trim();
    if path_text.is_empty() {
        return;
    }
    if let Some(import) = ctx
        .analysis
        .imports
        .iter()
        .find(|import| import.import.path.raw() == path_text)
    {
        if push_import_specs(ctx, import, out, seen) {
            return;
        }
    }
    if let Some(module) = resolve_module(ctx, path_text) {
        push_module_specs(ctx, &module, out, seen);
        return;
    }
    if let Some(uri) = derive_module_uri(ctx, path_text) {
        if let Some(analysis) = ctx.host.analyze(&uri) {
            push_top_level_specs(ctx, &analysis, 2, out, seen);
            for (path, decl) in collect_type_nodes(&analysis) {
                if path.len() == 1 {
                    push_type_node_specs(&analysis, decl, 2, out, seen);
                }
            }
        }
    }
}

fn push_import_specs(
    ctx: &FeatureContext<'_>,
    import: &crate::analysis::ResolvedImport,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) -> bool {
    if let Some(module) = &import.target {
        push_module_specs(ctx, module, out, seen);
        return true;
    }
    if let Some(uri) = import_uri(ctx, import) {
        if let Some(analysis) = ctx.host.analyze(&uri) {
            push_top_level_specs(ctx, &analysis, 2, out, seen);
            for (path, decl) in collect_type_nodes(&analysis) {
                if path.len() == 1 {
                    push_type_node_specs(&analysis, decl, 2, out, seen);
                }
            }
            return true;
        }
    }
    false
}

fn resolve_module(ctx: &FeatureContext<'_>, path: &str) -> Option<ModuleSummary> {
    if let Some(import) = ctx
        .analysis
        .imports
        .iter()
        .find(|import| import.import.path.raw() == path)
    {
        if let Some(module) = &import.target {
            return Some(module.clone());
        }
    }
    if let Some((package, rest)) = path.split_once('.') {
        let parts = rest.split('.').map(str::to_string).collect::<Vec<_>>();
        if let Some(module) = find_package_module(ctx, package, &parts) {
            return Some(module);
        }
    }
    find_workspace_module(ctx, path)
}

// ---------------------------------------------------------------------------
// Qualified completion (`Foo.`)
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Target {
    BuiltinModule(String),
    Module(ModuleSummary),
    Type { uri: Url, path: Vec<String> },
    Fields(Vec<FieldInfo>),
}

fn complete_qualified(
    ctx: &FeatureContext<'_>,
    parts: &[String],
    offset: u32,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) {
    let Some(target) = resolve_target(ctx, parts, offset) else {
        return;
    };
    push_target(ctx, target, out, seen);
}

fn resolve_target(ctx: &FeatureContext<'_>, parts: &[String], offset: u32) -> Option<Target> {
    if parts.is_empty() {
        return None;
    }
    let workspace = ctx.host.workspace();
    if parts.len() == 1 {
        let name = &parts[0];
        if ctx.config.completion.builtins && ctx.host.builtins().module(name).is_some() {
            return Some(Target::BuiltinModule(name.clone()));
        }
        if let Some(import) = ctx
            .analysis
            .imports
            .iter()
            .find(|import| import.qualifier == *name)
        {
            if let Some(target) = import_target(ctx, import) {
                return Some(target);
            }
        }
        // A type declared in the current module takes precedence over a
        // workspace module with the same basename (`Widget := ...` inside
        // `Widget.roc`, or `Pair` alongside other modules).
        if type_decl_at(ctx.analysis, parts).is_some() {
            return Some(Target::Type {
                uri: ctx.analysis.uri.clone(),
                path: parts.to_vec(),
            });
        }
        if let Some(module) = find_workspace_module(ctx, name) {
            return Some(Target::Module(module.clone()));
        }
        if workspace.package(name).is_some() {
            return Some(Target::Module(package_stub(name, &workspace)));
        }
        if header_dependency(ctx, name).is_some() {
            return Some(Target::Module(package_stub(name, &workspace)));
        }
        if let Some(type_ref) = binding_type(ctx, name, offset, 6) {
            return resolve_type_ref(ctx, &type_ref, &[]);
        }
        return None;
    }

    let first = &parts[0];
    let rest = &parts[1..];
    if workspace.package(first).is_some() || header_dependency(ctx, first).is_some() {
        let module = find_package_module(ctx, first, rest);
        if let Some(module) = module {
            return Some(Target::Module(module));
        }
        return None;
    }
    if let Some(import) = ctx
        .analysis
        .imports
        .iter()
        .find(|import| import.qualifier == *first)
    {
        if let Some(target) = import_target(ctx, import) {
            return resolve_target_rest(ctx, target, rest);
        }
    }
    if ctx.host.builtins().module(first).is_some() {
        let full = parts.join(".");
        let builtins = ctx.host.builtins();
        if let Some(item) = builtins.lookup(&full) {
            if item.kind == BuiltinKind::Type {
                return Some(Target::BuiltinModule(item.short_name.clone()));
            }
        }
        if parts.len() == 2 {
            if let Some(item) = builtins
                .items_in_module(first)
                .find(|item| item.short_name == parts[1])
            {
                if item.kind == BuiltinKind::Type {
                    return Some(Target::BuiltinModule(item.short_name.clone()));
                }
            }
        }
        return None;
    }
    if let Some(module) = find_workspace_module(ctx, first) {
        let mut path = vec![module
            .name
            .rsplit('/')
            .next()
            .unwrap_or(&module.name)
            .to_string()];
        path.extend(rest.iter().cloned());
        if type_decl_at_uri(ctx, &module.uri, &path).is_some() {
            return Some(Target::Type {
                uri: module.uri.clone(),
                path,
            });
        }
        return Some(Target::Module(module.clone()));
    }
    if type_decl_at(ctx.analysis, parts).is_some() {
        return Some(Target::Type {
            uri: ctx.analysis.uri.clone(),
            path: parts.to_vec(),
        });
    }
    // A local binding whose type is a named type: `order.status.`.
    if let Some(type_ref) = binding_type(ctx, first, offset, 6) {
        return resolve_type_ref(ctx, &type_ref, rest);
    }
    None
}

fn resolve_type_ref(
    ctx: &FeatureContext<'_>,
    type_ref: &TypeRef,
    rest: &[String],
) -> Option<Target> {
    match type_ref {
        TypeRef::InlineRecord(fields) if rest.is_empty() => Some(Target::Fields(fields.clone())),
        TypeRef::Named(path) => {
            let mut path = path.clone();
            path.extend(rest.iter().cloned());
            resolve_named_target(ctx, &path)
        }
        TypeRef::Unknown | TypeRef::InlineRecord(_) => None,
    }
}

fn resolve_named_target(ctx: &FeatureContext<'_>, path: &[String]) -> Option<Target> {
    if path.is_empty() {
        return None;
    }
    if type_decl_at(ctx.analysis, path).is_some() {
        return Some(Target::Type {
            uri: ctx.analysis.uri.clone(),
            path: path.to_vec(),
        });
    }
    if path.len() == 1
        && ctx.config.completion.builtins
        && ctx.host.builtins().module(&path[0]).is_some()
    {
        return Some(Target::BuiltinModule(path[0].clone()));
    }
    for import in &ctx.analysis.imports {
        if path[0] == import.qualifier {
            if let Some(target) = import_target(ctx, import) {
                if path.len() == 1 {
                    return Some(target);
                }
                return resolve_target_rest(ctx, target, &path[1..]);
            }
        }
        if path.len() == 1
            && import
                .exposing
                .iter()
                .any(|name| name.as_str() == path[0].as_str())
        {
            if let Some(uri) = import_uri(ctx, import) {
                if type_decl_at_uri(ctx, &uri, path).is_some() {
                    return Some(Target::Type {
                        uri,
                        path: path.to_vec(),
                    });
                }
            }
        }
    }
    if let Some(module) = find_workspace_module(ctx, &path[0]) {
        let mut candidate = vec![module
            .name
            .rsplit('/')
            .next()
            .unwrap_or(&module.name)
            .to_string()];
        candidate.extend(path[1..].iter().cloned());
        if type_decl_at_uri(ctx, &module.uri, &candidate).is_some() {
            return Some(Target::Type {
                uri: module.uri.clone(),
                path: candidate,
            });
        }
    }
    None
}

fn resolve_target_rest(
    ctx: &FeatureContext<'_>,
    target: Target,
    rest: &[String],
) -> Option<Target> {
    if rest.is_empty() {
        return Some(target);
    }
    match target {
        Target::Type { uri, mut path } => {
            path.extend(rest.iter().cloned());
            if type_decl_at_uri(ctx, &uri, &path).is_some() {
                Some(Target::Type { uri, path })
            } else {
                None
            }
        }
        Target::Module(module) => {
            if rest.len() == 1 && module.export(&rest[0]).is_some() {
                let path = vec![rest[0].clone()];
                let mut candidates = Vec::new();
                candidates.push(path.clone());
                let mut qualified = vec![module
                    .name
                    .rsplit('/')
                    .next()
                    .unwrap_or(&module.name)
                    .to_string()];
                qualified.extend(path.iter().cloned());
                candidates.push(qualified);
                for candidate in candidates {
                    if type_decl_at_uri(ctx, &module.uri, &candidate).is_some() {
                        return Some(Target::Type {
                            uri: module.uri.clone(),
                            path: candidate,
                        });
                    }
                }
                Some(Target::Module(module))
            } else {
                let mut candidates = vec![vec![module
                    .name
                    .rsplit('/')
                    .next()
                    .unwrap_or(&module.name)
                    .to_string()]];
                candidates[0].extend(rest.iter().cloned());
                candidates.push(rest.to_vec());
                for path in candidates {
                    if type_decl_at_uri(ctx, &module.uri, &path).is_some() {
                        return Some(Target::Type {
                            uri: module.uri.clone(),
                            path,
                        });
                    }
                }
                None
            }
        }
        Target::BuiltinModule(module) => {
            if rest.len() == 1 {
                let full = format!("{module}.{}", rest[0]);
                if let Some(item) = ctx.host.builtins().lookup(&full) {
                    if item.kind == BuiltinKind::Type {
                        return Some(Target::BuiltinModule(item.short_name.clone()));
                    }
                }
            }
            None
        }
        Target::Fields(_) => None,
    }
}

fn import_target(
    ctx: &FeatureContext<'_>,
    import: &crate::analysis::ResolvedImport,
) -> Option<Target> {
    if let Some(module) = &import.target {
        return Some(Target::Module(module.clone()));
    }
    if let Some(uri) = import_uri(ctx, import) {
        let path = vec![import
            .import
            .path
            .raw()
            .rsplit(['.', '/'])
            .next()
            .unwrap_or(import.import.path.raw())
            .to_string()];
        if type_decl_at_uri(ctx, &uri, &path).is_some() {
            return Some(Target::Type { uri, path });
        }
        return Some(Target::Module(ModuleSummary {
            uri: uri.clone(),
            path: uri.to_file_path().unwrap_or_default(),
            name: path[0].clone(),
            kind: crate::analysis::ModuleKind::Module,
            exports: Vec::new(),
            dependencies: Vec::new(),
            header_kind: None,
            header_range: None,
            doc: None,
            package: None,
            package_root: None,
        }));
    }
    None
}

fn import_uri(ctx: &FeatureContext<'_>, import: &crate::analysis::ResolvedImport) -> Option<Url> {
    import
        .file_uri
        .clone()
        .or_else(|| import.target.as_ref().map(|target| target.uri.clone()))
        .or_else(|| derive_module_uri(ctx, import.import.path.raw()))
}

fn derive_module_uri(ctx: &FeatureContext<'_>, raw: &str) -> Option<Url> {
    let path = ctx.analysis.source.path()?;
    let dir = path.parent()?;
    let mut candidates = Vec::new();
    candidates.push(dir.join(format!("{}.roc", raw)));
    candidates.push(dir.join(format!("{}.roc", raw.replace('.', "/"))));
    if raw.contains('.') {
        let last = raw.rsplit('.').next().unwrap_or(raw);
        candidates.push(dir.join(format!("{last}.roc")));
    }
    for candidate in candidates {
        if candidate.is_file() {
            return uri_for_path(&candidate);
        }
    }
    None
}

fn package_stub(name: &str, workspace: &crate::analysis::WorkspaceIndex) -> ModuleSummary {
    let path = workspace.root.join(format!("{name}.roc"));
    let uri = Url::from_file_path(&path)
        .unwrap_or_else(|_| Url::parse("file:///package").expect("static url"));
    ModuleSummary {
        uri,
        path,
        name: name.to_string(),
        kind: crate::analysis::ModuleKind::Package,
        exports: Vec::new(),
        dependencies: Vec::new(),
        header_kind: None,
        header_range: None,
        doc: None,
        package: Some(name.to_string()),
        package_root: None,
    }
}

fn find_package_module(
    ctx: &FeatureContext<'_>,
    package: &str,
    rest: &[String],
) -> Option<ModuleSummary> {
    let workspace = ctx.host.workspace();
    let dotted = rest.join(".");
    let slashed = rest.join("/");
    let names = [dotted.as_str(), slashed.as_str(), rest.last()?.as_str()];
    for name in names {
        if let Some(module) = workspace.find_module(Some(package), name) {
            return Some(module.clone());
        }
    }
    workspace
        .modules
        .iter()
        .find(|module| {
            module.package.as_deref() == Some(package)
                && module
                    .name
                    .rsplit('/')
                    .next()
                    .is_some_and(|last| rest.last().is_some_and(|name| name == last))
        })
        .cloned()
}

fn find_workspace_module(ctx: &FeatureContext<'_>, name: &str) -> Option<ModuleSummary> {
    let workspace = ctx.host.workspace();
    workspace
        .modules
        .iter()
        .find(|module| module.package.is_none() && module.name == name)
        .or_else(|| {
            workspace.modules.iter().find(|module| {
                module.package.is_none()
                    && (module.name.ends_with(&format!("/{name}"))
                        || module.name.rsplit('/').next() == Some(name))
            })
        })
        .cloned()
}

fn push_target(
    ctx: &FeatureContext<'_>,
    target: Target,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) {
    match target {
        Target::BuiltinModule(name) => push_builtin_module(ctx, &name, out, seen),
        Target::Module(module) => push_module_specs(ctx, &module, out, seen),
        Target::Type { uri, path } => {
            let analysis = if uri == ctx.analysis.uri {
                None
            } else {
                ctx.host.analyze(&uri)
            };
            let analysis_ref: &AnalyzedFile = analysis.as_deref().unwrap_or(ctx.analysis);
            if let Some(decl) = type_decl_at(analysis_ref, &path).or_else(|| {
                path.last()
                    .and_then(|name| type_decl_by_name(analysis_ref, name))
            }) {
                push_type_node_specs(analysis_ref, decl, 1, out, seen);
            }
        }
        Target::Fields(fields) => {
            for field in fields {
                push_spec(out, seen, field_spec(&field));
            }
        }
    }
}

fn push_builtin_module(
    ctx: &FeatureContext<'_>,
    name: &str,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) {
    let builtins = ctx.host.builtins();
    let Some(module) = builtins.module(name) else {
        return;
    };
    let module_doc = module.doc.clone();
    for item in &module.items {
        if item.name == module.name {
            continue;
        }
        let kind = match item.kind {
            BuiltinKind::Type => CompletionItemKind::STRUCT,
            BuiltinKind::Value => CompletionItemKind::FUNCTION,
            BuiltinKind::Tag => CompletionItemKind::ENUM_MEMBER,
        };
        let detail = item
            .signature
            .clone()
            .or_else(|| item.type_def.clone())
            .unwrap_or_else(|| item.kind_string().to_string());
        push_spec(
            out,
            seen,
            Spec::new(item.short_name.clone(), kind, 1)
                .detail(detail)
                .doc(
                    item.doc
                        .clone()
                        .or_else(|| module_doc.clone())
                        .unwrap_or_default(),
                )
                .data(json!({"kind": "builtin", "name": item.name})),
        );
    }
    // The module's own type is a member too (e.g. `Str`), but it rarely helps
    // after a dot; keep the list focused on callable members.
}

fn push_module_specs(
    ctx: &FeatureContext<'_>,
    module: &ModuleSummary,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) {
    for export in &module.exports {
        push_export_specs(module, export, 1, out, seen);
    }
    if let Some(analysis) = ctx.host.analyze(&module.uri) {
        for (path, decl) in collect_type_nodes(&analysis) {
            let last = module.name.rsplit('/').next().unwrap_or(&module.name);
            if path.len() == 1 && path[0] == last {
                push_type_node_specs(&analysis, decl, 1, out, seen);
            }
        }
        if module.exports.is_empty() {
            push_top_level_specs(ctx, &analysis, 1, out, seen);
        }
    }
}

fn push_export_specs(
    module: &ModuleSummary,
    export: &Export,
    group: u8,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) {
    let kind = completion_kind(export.kind);
    let detail = export
        .detail
        .clone()
        .unwrap_or_else(|| kind_label(export.kind).to_string());
    let data = json!({
        "kind": "symbol",
        "uri": module.uri.as_str(),
        "name": export.name,
        "container": export.container,
    });
    push_spec(
        out,
        seen,
        Spec::new(export.name.clone(), kind, group)
            .detail(detail)
            .doc(export.doc.clone().unwrap_or_default())
            .data(data),
    );
    for child in &export.children {
        push_export_specs(module, child, 1, out, seen);
    }
}

fn push_top_level_specs(
    ctx: &FeatureContext<'_>,
    analysis: &AnalyzedFile,
    group: u8,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) {
    let mut symbols = Vec::new();
    collect_symbols(&analysis.symbols, &mut symbols);
    for symbol in symbols {
        push_symbol_spec(analysis, symbol, group, out, seen);
    }
    for (path, decl) in collect_type_nodes(analysis) {
        if path.len() == 1 {
            push_spec(
                out,
                seen,
                Spec::new(decl.name.text.clone(), CompletionItemKind::STRUCT, group)
                    .detail(match decl.kind {
                        TypeDeclKind::Alias => "type alias",
                        TypeDeclKind::Nominal => "nominal type",
                        TypeDeclKind::Opaque => "opaque type",
                    })
                    .doc(
                        decl.doc
                            .as_ref()
                            .map(|doc| doc.text.clone())
                            .unwrap_or_default(),
                    )
                    .data(json!({
                        "kind": "symbol",
                        "uri": analysis.uri.as_str(),
                        "name": decl.name.text,
                    })),
            );
        }
    }
    let _ = ctx;
}

fn collect_symbols<'a>(symbols: &'a [SymbolInfo], out: &mut Vec<&'a SymbolInfo>) {
    for symbol in symbols {
        out.push(symbol);
        collect_symbols(&symbol.children, out);
    }
}

fn push_symbol_spec(
    analysis: &AnalyzedFile,
    symbol: &SymbolInfo,
    group: u8,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) {
    let detail = symbol
        .detail
        .clone()
        .unwrap_or_else(|| kind_label(symbol.kind).to_string());
    push_spec(
        out,
        seen,
        Spec::new(symbol.name.clone(), completion_kind(symbol.kind), group)
            .detail(detail)
            .doc(symbol.doc.clone().unwrap_or_default())
            .data(json!({
                "kind": "symbol",
                "uri": analysis.uri.as_str(),
                "name": symbol.name,
                "container": symbol.container,
            })),
    );
}

fn type_decl_at<'a>(analysis: &'a AnalyzedFile, target: &[String]) -> Option<&'a TypeDecl> {
    fn visit<'a>(
        items: &'a [Item],
        prefix: &mut Vec<String>,
        target: &[String],
    ) -> Option<&'a TypeDecl> {
        for item in items {
            if let ItemKind::TypeDecl(decl) = &item.kind {
                prefix.push(decl.name.text.clone());
                if prefix.as_slice() == target {
                    return Some(decl);
                }
                if let Some(found) = visit(&decl.members, prefix, target) {
                    return Some(found);
                }
                prefix.pop();
            }
        }
        None
    }
    let mut prefix = Vec::new();
    visit(&analysis.parsed.module.items, &mut prefix, target)
}

fn type_decl_by_name<'a>(analysis: &'a AnalyzedFile, name: &str) -> Option<&'a TypeDecl> {
    fn visit<'a>(items: &'a [Item], name: &str) -> Option<&'a TypeDecl> {
        for item in items {
            if let ItemKind::TypeDecl(decl) = &item.kind {
                if decl.name.text == name {
                    return Some(decl);
                }
                if let Some(found) = visit(&decl.members, name) {
                    return Some(found);
                }
            }
        }
        None
    }
    visit(&analysis.parsed.module.items, name)
}

fn type_decl_at_uri(ctx: &FeatureContext<'_>, uri: &Url, path: &[String]) -> Option<()> {
    if uri == &ctx.analysis.uri {
        return type_decl_at(ctx.analysis, path).map(|_| ());
    }
    let analysis = ctx.host.analyze(uri)?;
    type_decl_at(&analysis, path)
        .or_else(|| {
            path.last()
                .and_then(|name| type_decl_by_name(&analysis, name))
        })
        .map(|_| ())
}

fn collect_type_nodes(analysis: &AnalyzedFile) -> Vec<(Vec<String>, &TypeDecl)> {
    fn visit<'a>(
        items: &'a [Item],
        prefix: &mut Vec<String>,
        out: &mut Vec<(Vec<String>, &'a TypeDecl)>,
    ) {
        for item in items {
            if let ItemKind::TypeDecl(decl) = &item.kind {
                prefix.push(decl.name.text.clone());
                out.push((prefix.clone(), decl));
                visit(&decl.members, prefix, out);
                prefix.pop();
            }
        }
    }
    let mut out = Vec::new();
    let mut prefix = Vec::new();
    visit(&analysis.parsed.module.items, &mut prefix, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Type/field model
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FieldInfo {
    name: String,
    ty: Option<String>,
    optional: bool,
    default: Option<String>,
}

#[derive(Clone)]
enum TypeRef {
    Named(Vec<String>),
    InlineRecord(Vec<FieldInfo>),
    Unknown,
}

fn type_text(analysis: &AnalyzedFile, ty: &TypeExpr) -> String {
    analysis.source.slice(ty.range).trim().to_string()
}

fn field_info_from_type(analysis: &AnalyzedFile, field: &TypeField) -> FieldInfo {
    FieldInfo {
        name: field.name.text.clone(),
        ty: Some(type_text(analysis, &field.ty)),
        optional: field.optional,
        default: field
            .default
            .as_ref()
            .map(|expr| analysis.source.slice(expr.range).trim().to_string()),
    }
}

fn field_spec(field: &FieldInfo) -> Spec {
    let mut detail = field.ty.clone().unwrap_or_else(|| "field".to_string());
    if field.optional {
        detail.push_str(" (optional)");
    }
    if let Some(default) = &field.default {
        detail.push_str(&format!(" = {default}"));
    }
    let mut spec = Spec::new(field.name.clone(), CompletionItemKind::FIELD, 1)
        .detail(detail)
        .data(json!({"kind": "field", "name": field.name}));
    if field.optional {
        spec = spec.insert(format!("{}: ", field.name));
    }
    spec
}

fn tag_specs_from_type(analysis: &AnalyzedFile, tag: &TypeTag) -> Spec {
    let detail = if tag.payloads.is_empty() {
        "tag".to_string()
    } else {
        format!(
            "{}({})",
            tag.name.text,
            tag.payloads
                .iter()
                .map(|payload| type_text(analysis, payload))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Spec::new(tag.name.text.clone(), CompletionItemKind::ENUM_MEMBER, 1)
        .detail(detail)
        .data(json!({"kind": "tag", "name": tag.name.text}))
}

fn push_type_node_specs(
    analysis: &AnalyzedFile,
    decl: &TypeDecl,
    group: u8,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) {
    // Members first: a method and a field with the same name should complete as
    // the method.
    for item in &decl.members {
        match &item.kind {
            ItemKind::Definition(definition) => {
                if let Some(name) = definition.simple_name() {
                    let detail = definition
                        .annotation
                        .as_ref()
                        .map(|ty| type_text(analysis, ty))
                        .unwrap_or_else(|| "function".to_string());
                    push_spec(
                        out,
                        seen,
                        Spec::new(name.text.clone(), CompletionItemKind::FUNCTION, group)
                            .detail(detail)
                            .doc(
                                definition
                                    .doc
                                    .as_ref()
                                    .map(|doc| doc.text.clone())
                                    .unwrap_or_default(),
                            )
                            .data(json!({
                                "kind": "symbol",
                                "uri": analysis.uri.as_str(),
                                "name": name.text,
                                "container": decl.name.text,
                            })),
                    );
                }
            }
            ItemKind::TypeDecl(nested) => {
                push_spec(
                    out,
                    seen,
                    Spec::new(nested.name.text.clone(), CompletionItemKind::STRUCT, group)
                        .detail("nested type")
                        .doc(
                            nested
                                .doc
                                .as_ref()
                                .map(|doc| doc.text.clone())
                                .unwrap_or_default(),
                        )
                        .data(json!({
                            "kind": "symbol",
                            "uri": analysis.uri.as_str(),
                            "name": nested.name.text,
                            "container": decl.name.text,
                        })),
                );
            }
            ItemKind::Annotation(annotation) => {
                push_spec(
                    out,
                    seen,
                    Spec::new(
                        annotation.name.text.clone(),
                        CompletionItemKind::FUNCTION,
                        group,
                    )
                    .detail(type_text(analysis, &annotation.ty))
                    .doc(
                        annotation
                            .doc
                            .as_ref()
                            .map(|doc| doc.text.clone())
                            .unwrap_or_default(),
                    )
                    .data(json!({
                        "kind": "symbol",
                        "uri": analysis.uri.as_str(),
                        "name": annotation.name.text,
                        "container": decl.name.text,
                    })),
                );
            }
            _ => {}
        }
    }

    match &decl.backing.kind {
        TypeKind::Record { fields, .. } => {
            for field in fields {
                push_spec(
                    out,
                    seen,
                    field_spec(&field_info_from_type(analysis, field)),
                );
            }
        }
        TypeKind::TagUnion { tags, .. } => {
            for tag in tags {
                push_spec(out, seen, tag_specs_from_type(analysis, tag));
            }
        }
        TypeKind::Named(name) => {
            let path: Vec<String> = name.parts.iter().map(|part| part.text.clone()).collect();
            if path.len() != 1 || path[0] != decl.name.text {
                if let Some(nested) = type_decl_at(analysis, &path) {
                    push_type_node_specs(analysis, nested, group, out, seen);
                }
            }
        }
        TypeKind::Apply { name, .. } => {
            let path: Vec<String> = name.parts.iter().map(|part| part.text.clone()).collect();
            if let Some(nested) = type_decl_at(analysis, &path) {
                push_type_node_specs(analysis, nested, group, out, seen);
            }
        }
        _ => {}
    }
}

fn binding_type_from_detail(detail: Option<&str>) -> Option<TypeRef> {
    let detail = detail?.trim();
    if detail.is_empty() {
        return None;
    }
    if detail.starts_with('{') {
        return Some(TypeRef::InlineRecord(parse_record_text(detail)));
    }
    if detail.contains("->") || detail.contains("=>") {
        return None;
    }
    let mut end = 0usize;
    for (index, ch) in detail.char_indices() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '/' | '$') {
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    let head = &detail[..end];
    if head.starts_with(|ch: char| ch.is_ascii_uppercase()) {
        let parts = head.split('.').map(str::to_string).collect::<Vec<_>>();
        Some(TypeRef::Named(parts))
    } else {
        None
    }
}

/// Convert a function return type to a receiver type.
///
/// Builtin signatures often append a top-level `where` clause:
/// `Dict.insert : Dict(k, v), k, v -> Dict(k, v) where [k.is_eq : ...]`.
/// The clause can itself contain arrows, so it must be removed before deciding
/// whether the return value is a named type whose members can be completed.
fn return_type_ref(text: &str) -> Option<TypeRef> {
    let mut depth = 0i32;
    let mut cut = text.len();
    for (index, ch) in text.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ if depth == 0
                && text[index..].starts_with("where")
                && (index == 0
                    || text[..index]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace))
                && text[index + "where".len()..]
                    .chars()
                    .next()
                    .map_or(true, |next| next.is_whitespace() || next == '[') =>
            {
                cut = index;
                break;
            }
            _ => {}
        }
    }
    binding_type_from_detail(Some(text[..cut].trim()))
}

fn parse_record_text(text: &str) -> Vec<FieldInfo> {
    let inner = text
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .trim();
    if inner.is_empty() {
        return Vec::new();
    }
    let mut fields = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut pieces = Vec::new();
    for (index, ch) in inner.char_indices() {
        match ch {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                pieces.push(&inner[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    pieces.push(&inner[start..]);
    for piece in pieces {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        if let Some((name, rest)) = piece.split_once(':') {
            let name = name.trim().trim_end_matches('?').trim().to_string();
            if name.is_empty()
                || !name
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_lowercase())
            {
                continue;
            }
            let optional = piece
                .split_once(':')
                .is_some_and(|(left, _)| left.trim().ends_with('?'));
            let (ty, default) = match rest.split_once("??") {
                Some((ty, default)) => (
                    Some(ty.trim().to_string()),
                    Some(default.trim().to_string()),
                ),
                None => (Some(rest.trim().to_string()), None),
            };
            fields.push(FieldInfo {
                name,
                ty,
                optional,
                default,
            });
        }
    }
    fields
}

fn fields_from_analysis(analysis: &AnalyzedFile, path: &[String]) -> Option<Vec<FieldInfo>> {
    let decl = type_decl_at(analysis, path)?;
    match &decl.backing.kind {
        TypeKind::Record { fields, .. } => Some(
            fields
                .iter()
                .map(|field| field_info_from_type(analysis, field))
                .collect(),
        ),
        TypeKind::Named(name) => {
            let next: Vec<String> = name.parts.iter().map(|part| part.text.clone()).collect();
            if next == path {
                None
            } else {
                fields_from_analysis(analysis, &next)
            }
        }
        _ => None,
    }
}

fn fields_for_type_ref(ctx: &FeatureContext<'_>, type_ref: &TypeRef) -> Option<Vec<FieldInfo>> {
    match type_ref {
        TypeRef::InlineRecord(fields) => Some(fields.clone()),
        TypeRef::Named(path) => {
            if let Some(fields) = fields_from_analysis(ctx.analysis, path) {
                return Some(fields);
            }
            for import in &ctx.analysis.imports {
                if path.first().is_some_and(|first| *first == import.qualifier) {
                    if let Some(uri) = import_uri(ctx, import) {
                        if let Some(analysis) = ctx.host.analyze(&uri) {
                            // `Widget` denotes the imported module's own type
                            // (`Widget := ...`), not a member of an empty path.
                            let type_path = if path.len() == 1 {
                                path.as_slice()
                            } else {
                                &path[1..]
                            };
                            if let Some(fields) = fields_from_analysis(&analysis, type_path) {
                                return Some(fields);
                            }
                        }
                    }
                }
                if path.len() == 1
                    && import
                        .exposing
                        .iter()
                        .any(|name| name.as_str() == path[0].as_str())
                {
                    if let Some(uri) = import_uri(ctx, import) {
                        if let Some(analysis) = ctx.host.analyze(&uri) {
                            if let Some(fields) = fields_from_analysis(&analysis, path) {
                                return Some(fields);
                            }
                        }
                    }
                }
            }
            if let Some(module) = find_workspace_module(ctx, &path[0]) {
                if let Some(analysis) = ctx.host.analyze(&module.uri) {
                    let mut candidate = vec![module
                        .name
                        .rsplit('/')
                        .next()
                        .unwrap_or(&module.name)
                        .to_string()];
                    candidate.extend(path[1..].iter().cloned());
                    if let Some(fields) = fields_from_analysis(&analysis, &candidate) {
                        return Some(fields);
                    }
                }
            }
            None
        }
        TypeRef::Unknown => None,
    }
}

// ---------------------------------------------------------------------------
// Binding/expression type inference (conservative)
// ---------------------------------------------------------------------------

fn type_ref_text(ctx: &FeatureContext<'_>, expr: &Expr, depth: u8) -> Option<String> {
    match infer_expr_type(ctx, expr, depth)? {
        TypeRef::Named(parts) => Some(parts.join(".")),
        TypeRef::InlineRecord(_) => Some("record".to_string()),
        TypeRef::Unknown => None,
    }
}

fn infer_expr_type(ctx: &FeatureContext<'_>, expr: &Expr, depth: u8) -> Option<TypeRef> {
    if depth == 0 {
        return None;
    }
    match &expr.kind {
        ExprKind::Record { fields, .. } => Some(TypeRef::InlineRecord(
            fields
                .iter()
                .map(|field| FieldInfo {
                    name: field.name.text.clone(),
                    ty: field
                        .value
                        .as_ref()
                        .and_then(|value| type_ref_text(ctx, value, depth - 1)),
                    optional: field.optional,
                    default: None,
                })
                .collect(),
        )),
        ExprKind::Nominal { type_name, .. } => Some(TypeRef::Named(
            type_name
                .parts
                .iter()
                .map(|part| part.text.clone())
                .collect(),
        )),
        ExprKind::Call { callee, .. } => resolve_call_signature(ctx, callee, false)
            .and_then(|resolved| resolved.signature.return_type)
            .and_then(|text| return_type_ref(&text)),
        ExprKind::MethodCall {
            receiver, method, ..
        } => {
            let receiver_type =
                infer_expr_type(ctx, receiver, depth - 1).unwrap_or(TypeRef::Unknown);
            method_signature_for_type(ctx, &receiver_type, method.text.as_str())
                .or_else(|| method_signature(ctx, method.text.as_str()))
                .and_then(|signature| signature.return_type)
                .and_then(|text| return_type_ref(&text))
        }
        ExprKind::Field { base, field, .. } => {
            let base_type = infer_expr_type(ctx, base, depth - 1)?;
            match base_type {
                TypeRef::InlineRecord(fields) => fields
                    .iter()
                    .find(|candidate| candidate.name == field.text)
                    .and_then(|candidate| candidate.ty.as_deref().and_then(return_type_ref)),
                TypeRef::Named(_) => fields_for_type_ref(ctx, &base_type)
                    .and_then(|fields| {
                        fields
                            .into_iter()
                            .find(|candidate| candidate.name == field.text)
                    })
                    .and_then(|candidate| candidate.ty.as_deref().and_then(return_type_ref)),
                TypeRef::Unknown => None,
            }
        }
        ExprKind::Paren(inner) => infer_expr_type(ctx, inner, depth - 1),
        ExprKind::If { then_branch, .. } => infer_expr_type(ctx, then_branch, depth - 1),
        ExprKind::Lookup { path } if path.len() == 1 => {
            binding_type(ctx, &path[0].text, expr.range.start, depth - 1)
        }
        _ => None,
    }
}

fn binding_type(ctx: &FeatureContext<'_>, name: &str, offset: u32, depth: u8) -> Option<TypeRef> {
    if depth == 0 {
        return None;
    }
    if let Some(symbol) = ctx
        .analysis
        .visible_bindings(offset)
        .into_iter()
        .find(|symbol| symbol.name == name)
    {
        if let Some(type_ref) = binding_type_from_detail(symbol.detail.as_deref()) {
            return Some(type_ref);
        }
        if let Some(definition) = local_definition(
            ctx.analysis,
            &symbol.name,
            symbol.name_range,
            symbol.full_range,
        ) {
            if let Some(annotation) = &definition.annotation {
                if let Some(type_ref) = type_ref_from_annotation(ctx.analysis, annotation) {
                    return Some(type_ref);
                }
            }
            if let Some(type_ref) = infer_expr_type(ctx, &definition.value, depth - 1) {
                return Some(type_ref);
            }
        }
    }
    let definitions = all_definitions(ctx.analysis);
    for (container, definition) in &definitions {
        let Some(pattern_name) = definition.simple_name() else {
            continue;
        };
        if pattern_name.text != name {
            continue;
        }
        let _ = container;
        if let Some(annotation) = &definition.annotation {
            if let Some(type_ref) = type_ref_from_annotation(ctx.analysis, annotation) {
                return Some(type_ref);
            }
        }
        if let Some(type_ref) = infer_expr_type(ctx, &definition.value, depth - 1) {
            return Some(type_ref);
        }
    }
    // Conservative fallback: a parameter with no annotation whose fields are
    // read nearby gets exactly those fields.
    let scope_range = ctx
        .analysis
        .enclosing_scope(offset)
        .map(|scope| scope.range)
        .unwrap_or(TextRange::new(0, ctx.analysis.source.len()));
    let mut fields: Vec<FieldInfo> = Vec::new();

    fn collect_expr(expr: &Expr, name: &str, range: TextRange, fields: &mut Vec<FieldInfo>) {
        if !expr.range.intersects(range) {
            return;
        }
        match &expr.kind {
            ExprKind::Field { base, field, .. } => {
                if let ExprKind::Lookup { path } = &base.kind {
                    if path.len() == 1
                        && path[0].text == name
                        && !fields.iter().any(|existing| existing.name == field.text)
                    {
                        fields.push(FieldInfo {
                            name: field.text.clone(),
                            ty: None,
                            optional: false,
                            default: None,
                        });
                    }
                }
                collect_expr(base, name, range, fields);
            }
            ExprKind::Call { callee, args } => {
                collect_expr(callee, name, range, fields);
                for arg in args {
                    collect_expr(arg, name, range, fields);
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                collect_expr(receiver, name, range, fields);
                for arg in args {
                    collect_expr(arg, name, range, fields);
                }
            }
            ExprKind::List(items) | ExprKind::Tuple(items) => {
                for item in items {
                    collect_expr(item, name, range, fields);
                }
            }
            ExprKind::Record {
                fields: record,
                update,
            } => {
                for field in record {
                    if let Some(value) = &field.value {
                        collect_expr(value, name, range, fields);
                    }
                }
                if let Some(update) = update {
                    collect_expr(update, name, range, fields);
                }
            }
            ExprKind::Lambda { body, .. } => collect_expr(body, name, range, fields),
            ExprKind::Block { stmts, tail } => {
                for stmt in stmts {
                    collect_stmt(stmt, name, range, fields);
                }
                if let Some(tail) = tail {
                    collect_expr(tail, name, range, fields);
                }
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                collect_expr(condition, name, range, fields);
                collect_expr(then_branch, name, range, fields);
                collect_expr(else_branch, name, range, fields);
            }
            ExprKind::Match { subject, branches } => {
                collect_expr(subject, name, range, fields);
                for branch in branches {
                    if let Some(guard) = &branch.guard {
                        collect_expr(guard, name, range, fields);
                    }
                    collect_expr(&branch.body, name, range, fields);
                }
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                collect_expr(lhs, name, range, fields);
                collect_expr(rhs, name, range, fields);
            }
            ExprKind::Unary { expr, .. }
            | ExprKind::Paren(expr)
            | ExprKind::Db(expr)
            | ExprKind::Crash(expr) => collect_expr(expr, name, range, fields),
            ExprKind::Tag { args, .. } => {
                for arg in args {
                    collect_expr(arg, name, range, fields);
                }
            }
            ExprKind::For { iterable, body, .. } => {
                collect_expr(iterable, name, range, fields);
                collect_expr(body, name, range, fields);
            }
            ExprKind::While { condition, body } => {
                collect_expr(condition, name, range, fields);
                collect_expr(body, name, range, fields);
            }
            ExprKind::Return(Some(value)) => collect_expr(value, name, range, fields),
            ExprKind::TupleIndex { base, .. } => collect_expr(base, name, range, fields),
            ExprKind::Nominal { value, .. } => collect_expr(value, name, range, fields),
            ExprKind::String(parts) | ExprKind::MultilineString(parts) => {
                for part in parts {
                    if let crate::syntax::ast::StrPartKind::Interpolation(inner) = &part.kind {
                        collect_expr(inner, name, range, fields);
                    }
                }
            }
            ExprKind::Number(_)
            | ExprKind::Char(_)
            | ExprKind::Lookup { .. }
            | ExprKind::Return(None)
            | ExprKind::Break
            | ExprKind::Continue
            | ExprKind::Error => {}
        }
    }

    fn collect_stmt(
        stmt: &crate::syntax::ast::Stmt,
        name: &str,
        range: TextRange,
        fields: &mut Vec<FieldInfo>,
    ) {
        match &stmt.kind {
            StmtKind::Definition(definition) => {
                collect_expr(&definition.value, name, range, fields)
            }
            StmtKind::TypeDecl(_) => {}
            StmtKind::Assignment { value, .. } => collect_expr(value, name, range, fields),
            StmtKind::Db(value) | StmtKind::Expect(value) | StmtKind::Crash(value) => {
                collect_expr(value, name, range, fields)
            }
            StmtKind::Return(Some(value)) => collect_expr(value, name, range, fields),
            StmtKind::Expr(value) => collect_expr(value, name, range, fields),
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        }
    }

    fn collect_items(items: &[Item], name: &str, range: TextRange, fields: &mut Vec<FieldInfo>) {
        for item in items {
            match &item.kind {
                ItemKind::Definition(definition) => {
                    collect_expr(&definition.value, name, range, fields)
                }
                ItemKind::TypeDecl(decl) => collect_items(&decl.members, name, range, fields),
                ItemKind::Expect(expect) => collect_expr(&expect.expr, name, range, fields),
                _ => {}
            }
        }
    }

    collect_items(
        &ctx.analysis.parsed.module.items,
        name,
        scope_range,
        &mut fields,
    );
    if !fields.is_empty() {
        return Some(TypeRef::InlineRecord(fields));
    }
    None
}

fn type_ref_from_annotation(analysis: &AnalyzedFile, ty: &TypeExpr) -> Option<TypeRef> {
    match &ty.kind {
        TypeKind::Named(name) => Some(TypeRef::Named(
            name.parts.iter().map(|part| part.text.clone()).collect(),
        )),
        TypeKind::Apply { name, .. } => Some(TypeRef::Named(
            name.parts.iter().map(|part| part.text.clone()).collect(),
        )),
        TypeKind::Record { fields, .. } => Some(TypeRef::InlineRecord(
            fields
                .iter()
                .map(|field| field_info_from_type(analysis, field))
                .collect(),
        )),
        _ => None,
    }
}

fn all_definitions(analysis: &AnalyzedFile) -> Vec<(Option<String>, &Definition)> {
    fn visit<'a>(
        items: &'a [Item],
        container: Option<&str>,
        out: &mut Vec<(Option<String>, &'a Definition)>,
    ) {
        for item in items {
            match &item.kind {
                ItemKind::Definition(definition) => {
                    out.push((container.map(str::to_string), definition))
                }
                ItemKind::TypeDecl(decl) => visit(&decl.members, Some(&decl.name.text), out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    visit(&analysis.parsed.module.items, None, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Signatures
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ResolvedCall {
    signature: Signature,
    method: bool,
}

fn split_top_level(text: &str, separator: char) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (index, ch) in text.char_indices() {
        match ch {
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => depth -= 1,
            _ if ch == separator && depth == 0 => {
                pieces.push(text[start..index].trim().to_string());
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    pieces.push(text[start..].trim().to_string());
    pieces
}

fn split_signature_arrow(text: &str) -> Option<(String, Option<String>, bool)> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut arrow: Option<(usize, usize, bool)> = None;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'(' | b'{' | b'[' => depth += 1,
            b')' | b'}' | b']' => depth -= 1,
            b'-' if depth == 0 && bytes.get(index + 1) == Some(&b'>') => {
                arrow = Some((index, index + 2, false));
            }
            b'=' if depth == 0 && bytes.get(index + 1) == Some(&b'>') => {
                arrow = Some((index, index + 2, true));
            }
            _ => {}
        }
        index += 1;
    }
    let (start, end, effectful) = arrow?;
    Some((
        text[..start].trim().to_string(),
        Some(text[end..].trim().to_string()),
        effectful,
    ))
}

fn parse_signature_text(name: &str, text: &str) -> Option<Signature> {
    let text = text.trim();
    let (left, right, effectful) = split_signature_arrow(text)?;
    let mut params = Vec::new();
    for piece in split_top_level(&left, ',') {
        if piece.is_empty() {
            continue;
        }
        let (param_name, ty) = match split_top_level(&piece, ':').as_slice() {
            [name, ty] => (name.clone(), Some(ty.clone())),
            _ => (String::new(), Some(piece.clone())),
        };
        params.push(ParamInfo {
            name: param_name,
            ty,
            name_range: TextRange::default(),
        });
    }
    Some(Signature {
        name: name.to_string(),
        params,
        return_type: right,
        display: text.to_string(),
        is_effectful: effectful,
        type_params: Vec::new(),
        doc: None,
        key: DefinitionKey::Builtin {
            full_name: name.to_string(),
        },
    })
}

fn signature_from_builtin(item: &BuiltinItem) -> Option<Signature> {
    let display = item.signature.clone().or_else(|| item.type_def.clone())?;
    let mut signature = parse_signature_text(&item.short_name, &display)?;
    signature.display = display;
    signature.doc = item.doc.clone();
    signature.key = item.key();
    Some(signature)
}

fn signature_information(
    signature: &Signature,
    hide_receiver: usize,
    offset: usize,
    commas: u32,
) -> SignatureInformation {
    let start = hide_receiver.min(signature.params.len());
    let params = &signature.params[start..];
    let labels: Vec<String> = params
        .iter()
        .map(|param| match (&param.name, &param.ty) {
            (name, Some(ty)) if !name.is_empty() && name != "_" => format!("{name}: {ty}"),
            (_, Some(ty)) => ty.clone(),
            (name, None) if !name.is_empty() => name.clone(),
            _ => "_".to_string(),
        })
        .collect();
    let use_display = !signature.display.is_empty()
        && labels.iter().all(|label| signature.display.contains(label))
        && labels.len() == signature.params.len().saturating_sub(start);
    let label = if use_display {
        signature.display.clone()
    } else {
        let arrow = if signature.is_effectful { "=>" } else { "->" };
        let mut label = format!("{}({})", signature.name, labels.join(", "));
        if let Some(ret) = &signature.return_type {
            label.push_str(&format!(" {arrow} {ret}"));
        }
        label
    };
    let parameter_infos = if labels.is_empty() {
        None
    } else {
        Some(
            labels
                .iter()
                .map(|label| ParameterInformation {
                    label: ParameterLabel::Simple(label.clone()),
                    documentation: None,
                })
                .collect(),
        )
    };
    let active = if labels.is_empty() {
        None
    } else {
        let raw = commas as usize + offset.saturating_sub(start);
        Some((raw.min(labels.len() - 1)) as u32)
    };
    SignatureInformation {
        label,
        documentation: signature
            .doc
            .clone()
            .map(|doc| Documentation::MarkupContent(util::markdown(doc))),
        parameters: parameter_infos,
        active_parameter: active,
    }
}

fn signature_from_definition(
    _ctx: &FeatureContext<'_>,
    analysis: &AnalyzedFile,
    definition: &Definition,
    local: bool,
    container: Option<&str>,
) -> Option<Signature> {
    let name = definition.simple_name()?.text.clone();
    let annotation = definition.annotation.as_ref()?;
    let TypeKind::Function {
        args,
        ret,
        effectful,
    } = &annotation.kind
    else {
        return None;
    };
    let lambda_params = match &definition.value.kind {
        ExprKind::Lambda { params, .. } => Some(params),
        _ => None,
    };
    let params = args
        .iter()
        .enumerate()
        .map(|(index, ty)| {
            let pattern = lambda_params.and_then(|params| params.get(index));
            let (param_name, name_range) = match pattern {
                Some(Pattern {
                    kind: PatternKind::Name { name, .. },
                    ..
                }) => (name.text.clone(), name.range),
                _ => (String::new(), TextRange::default()),
            };
            ParamInfo {
                name: param_name,
                ty: Some(type_text(analysis, ty)),
                name_range,
            }
        })
        .collect::<Vec<_>>();
    let key = if local {
        DefinitionKey::Local {
            uri: analysis.uri.clone(),
            def_start: definition.pattern.range.start,
        }
    } else {
        DefinitionKey::TopLevel {
            uri: analysis.uri.clone(),
            name: name.clone(),
            container: container.map(str::to_string),
        }
    };
    Some(Signature {
        name: name.clone(),
        params,
        return_type: Some(type_text(analysis, ret)),
        display: format!("{name} : {}", type_text(analysis, annotation)),
        is_effectful: *effectful,
        type_params: Vec::new(),
        doc: definition.doc.as_ref().map(|doc| doc.text.clone()),
        key,
    })
}

fn signature_for_key(ctx: &FeatureContext<'_>, key: &DefinitionKey) -> Option<Signature> {
    match key {
        DefinitionKey::Builtin { full_name } => ctx
            .host
            .builtins()
            .lookup(full_name)
            .and_then(signature_from_builtin),
        DefinitionKey::Local { uri, def_start } => {
            let analysis = if uri == &ctx.analysis.uri {
                None
            } else {
                ctx.host.analyze(uri)
            };
            let analysis_ref: &AnalyzedFile = analysis.as_deref().unwrap_or(ctx.analysis);
            analysis_ref.signature_for(key).cloned().or_else(|| {
                analysis_ref
                    .symbols
                    .iter()
                    .find(|symbol| symbol.name_range.start == *def_start)
                    .and_then(|symbol| symbol.detail.as_deref())
                    .and_then(|detail| parse_signature_text(&key_name(key), detail))
            })
        }
        DefinitionKey::TopLevel {
            uri,
            name,
            container,
        } => {
            let analysis = if uri == &ctx.analysis.uri {
                None
            } else {
                ctx.host.analyze(uri)
            };
            let analysis_ref: &AnalyzedFile = analysis.as_deref().unwrap_or(ctx.analysis);
            if let Some(signature) = analysis_ref.signature_for(key) {
                return Some(signature.clone());
            }
            let definitions = all_definitions(analysis_ref);
            for (def_container, definition) in definitions {
                let Some(def_name) = definition.simple_name() else {
                    continue;
                };
                if def_name.text != *name {
                    continue;
                }
                if container
                    .as_deref()
                    .is_some_and(|wanted| def_container.as_deref() != Some(wanted))
                {
                    continue;
                }
                if let Some(signature) = signature_from_definition(
                    ctx,
                    analysis_ref,
                    definition,
                    false,
                    def_container.as_deref(),
                ) {
                    return Some(signature);
                }
            }
            analysis_ref
                .symbols
                .iter()
                .find(|symbol| symbol.name == *name)
                .and_then(|symbol| symbol.detail.as_deref())
                .and_then(|detail| parse_signature_text(name, detail))
        }
        DefinitionKey::Module { .. } => None,
    }
}

fn key_name(key: &DefinitionKey) -> String {
    match key {
        DefinitionKey::Builtin { full_name } => full_name.clone(),
        DefinitionKey::Local { .. } => "local".to_string(),
        DefinitionKey::TopLevel { name, .. } => name.clone(),
        DefinitionKey::Module { .. } => "module".to_string(),
    }
}

fn signature_for_name(
    ctx: &FeatureContext<'_>,
    name: &str,
    local: bool,
    container: Option<&str>,
) -> Option<Signature> {
    let mut fallback_signature: Option<Signature> = None;
    for signature in ctx.analysis.signatures.values() {
        if signature.name == name
            && container
                .map(|wanted| signature.key.is_writable_in(&ctx.analysis.uri) && wanted == name)
                .unwrap_or(true)
        {
            if signature.params.iter().any(|param| !param.name.is_empty()) {
                return Some(signature.clone());
            }
            fallback_signature = Some(signature.clone());
        }
    }
    if let Some(symbol) = find_symbol(ctx.analysis, name, container) {
        if let Some(signature) = ctx.analysis.signature_for(&symbol.key) {
            if signature.params.iter().any(|param| !param.name.is_empty()) {
                return Some(signature.clone());
            }
            fallback_signature = Some(signature.clone());
        }
    }
    for (def_container, definition) in all_definitions(ctx.analysis) {
        let Some(def_name) = definition.simple_name() else {
            continue;
        };
        if def_name.text != name {
            continue;
        }
        if let Some(wanted) = container {
            if def_container.as_deref() != Some(wanted) {
                continue;
            }
        }
        if let Some(signature) = signature_from_definition(
            ctx,
            ctx.analysis,
            definition,
            local,
            def_container.as_deref(),
        ) {
            return Some(signature);
        }
        if let Some(annotation) = &definition.annotation {
            let annotation_text = type_text(ctx.analysis, annotation);
            if let Some(mut signature) = parse_signature_text(name, &annotation_text) {
                signature.display = format!("{name} : {annotation_text}");
                signature.doc = definition.doc.as_ref().map(|doc| doc.text.clone());
                return Some(signature);
            }
        }
    }
    // Imported exposed names.
    for import in &ctx.analysis.imports {
        if import
            .exposing
            .iter()
            .any(|exposed| exposed.as_str() == name)
        {
            if let Some(module) = &import.target {
                if let Some(export) = module.export(name) {
                    if let Some(signature) = signature_for_export(ctx, module, export) {
                        return Some(signature);
                    }
                }
            }
            if let Some(uri) = import_uri(ctx, import) {
                if let Some(analysis) = ctx.host.analyze(&uri) {
                    for (def_container, definition) in all_definitions(&analysis) {
                        let Some(def_name) = definition.simple_name() else {
                            continue;
                        };
                        if def_name.text == name {
                            if let Some(signature) = signature_from_definition(
                                ctx,
                                &analysis,
                                definition,
                                false,
                                def_container.as_deref(),
                            ) {
                                return Some(signature);
                            }
                        }
                    }
                }
            }
        }
    }
    fallback_signature
}

fn signature_for_export(
    ctx: &FeatureContext<'_>,
    module: &ModuleSummary,
    export: &Export,
) -> Option<Signature> {
    if let Some(key) = &export.key {
        if let Some(signature) = signature_for_key(ctx, key) {
            return Some(signature);
        }
    }
    if let Some(detail) = &export.detail {
        if let Some(mut signature) = parse_signature_text(&export.name, detail) {
            signature.doc = export.doc.clone();
            signature.display = detail.clone();
            return Some(signature);
        }
    }
    if let Some(analysis) = ctx.host.analyze(&module.uri) {
        if let Some(signature) =
            signature_for_name_in(&analysis, &export.name, export.container.as_deref())
        {
            return Some(signature);
        }
    }
    None
}

fn signature_for_name_in(
    analysis: &AnalyzedFile,
    name: &str,
    container: Option<&str>,
) -> Option<Signature> {
    for (def_container, definition) in all_definitions(analysis) {
        let Some(def_name) = definition.simple_name() else {
            continue;
        };
        if def_name.text != name {
            continue;
        }
        if let Some(wanted) = container {
            if def_container.as_deref() != Some(wanted) {
                continue;
            }
        }
        if let Some(annotation) = &definition.annotation {
            let text = type_text(analysis, annotation);
            if let Some(mut signature) = parse_signature_text(name, &text) {
                signature.display = format!("{name} : {text}");
                signature.doc = definition.doc.as_ref().map(|doc| doc.text.clone());
                return Some(signature);
            }
        }
    }
    None
}

fn method_signature(ctx: &FeatureContext<'_>, name: &str) -> Option<Signature> {
    for (_path, decl) in collect_type_nodes(ctx.analysis) {
        for item in &decl.members {
            if let ItemKind::Definition(definition) = &item.kind {
                if definition
                    .simple_name()
                    .is_some_and(|def_name| def_name.text == name)
                {
                    if let Some(signature) = signature_from_definition(
                        ctx,
                        ctx.analysis,
                        definition,
                        false,
                        Some(&decl.name.text),
                    ) {
                        return Some(signature);
                    }
                }
            }
        }
    }
    ctx.host
        .builtins()
        .lookup(name)
        .and_then(signature_from_builtin)
}

fn method_signature_for_type(
    ctx: &FeatureContext<'_>,
    type_ref: &TypeRef,
    name: &str,
) -> Option<Signature> {
    if let TypeRef::Named(path) = type_ref {
        // Builtin type methods are fully qualified.
        if path.len() == 1 {
            let full = format!("{}.{}", path[0], name);
            if let Some(item) = ctx.host.builtins().lookup(&full) {
                return signature_from_builtin(item);
            }
        }
        if let Some(decl) = type_decl_at(ctx.analysis, path) {
            for item in &decl.members {
                if let ItemKind::Definition(definition) = &item.kind {
                    if definition
                        .simple_name()
                        .is_some_and(|def_name| def_name.text == name)
                    {
                        return signature_from_definition(
                            ctx,
                            ctx.analysis,
                            definition,
                            false,
                            Some(&decl.name.text),
                        );
                    }
                }
            }
        }
        for import in &ctx.analysis.imports {
            if path.first().is_some_and(|first| *first == import.qualifier) {
                if let Some(uri) = import_uri(ctx, import) {
                    if let Some(analysis) = ctx.host.analyze(&uri) {
                        // An imported module's own type shares the qualifier
                        // (`Widget := ...` in `Widget.roc`), so a one-segment
                        // type like `Widget` must be looked up as-is rather
                        // than as an empty path.
                        let type_path = if path.len() == 1 {
                            path.as_slice()
                        } else {
                            &path[1..]
                        };
                        if let Some(decl) = type_decl_at(&analysis, type_path) {
                            for item in &decl.members {
                                if let ItemKind::Definition(definition) = &item.kind {
                                    if definition
                                        .simple_name()
                                        .is_some_and(|def_name| def_name.text == name)
                                    {
                                        return signature_from_definition(
                                            ctx,
                                            &analysis,
                                            definition,
                                            false,
                                            Some(&decl.name.text),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    method_signature(ctx, name)
}

fn resolve_call_signature(
    ctx: &FeatureContext<'_>,
    callee: &Expr,
    _method: bool,
) -> Option<ResolvedCall> {
    match &callee.kind {
        ExprKind::Lookup { path } => {
            let parts = path
                .iter()
                .map(|part| part.text.clone())
                .collect::<Vec<_>>();
            resolve_chain_signature(ctx, &parts, None, callee.range.start)
        }
        ExprKind::Field { base, field, .. } => {
            let type_ref = infer_expr_type(ctx, base, 6).unwrap_or(TypeRef::Unknown);
            let signature = method_signature_for_type(ctx, &type_ref, &field.text)?;
            Some(ResolvedCall {
                signature,
                method: true,
            })
        }
        _ => None,
    }
}

fn resolve_chain_signature(
    ctx: &FeatureContext<'_>,
    parts: &[String],
    receiver: Option<&str>,
    offset: u32,
) -> Option<ResolvedCall> {
    if parts.is_empty() {
        return None;
    }
    let workspace = ctx.host.workspace();
    if parts.len() >= 2 {
        let full = parts.join(".");
        if let Some(item) = ctx.host.builtins().lookup(&full) {
            return signature_from_builtin(item).map(|signature| ResolvedCall {
                signature,
                method: false,
            });
        }
        let first = &parts[0];
        if workspace.package(first).is_some() || header_dependency(ctx, first).is_some() {
            if let Some(module) = find_package_module(ctx, first, &parts[1..]) {
                if let Some(signature) = signature_for_module_member(ctx, &module, parts.last()?) {
                    return Some(ResolvedCall {
                        signature,
                        method: false,
                    });
                }
            }
        }
        if let Some(import) = ctx
            .analysis
            .imports
            .iter()
            .find(|import| import.qualifier == *first)
        {
            if let Some(target) = import_target(ctx, import) {
                if let Some(signature) = signature_for_target_member(ctx, target, &parts[1..]) {
                    return Some(ResolvedCall {
                        signature,
                        method: false,
                    });
                }
            }
        }
        if let Some(decl) = type_decl_at(ctx.analysis, &parts[..parts.len() - 1]) {
            let name = parts.last()?;
            for item in &decl.members {
                if let ItemKind::Definition(definition) = &item.kind {
                    if definition
                        .simple_name()
                        .is_some_and(|def_name| def_name.text == *name)
                    {
                        if let Some(signature) = signature_from_definition(
                            ctx,
                            ctx.analysis,
                            definition,
                            false,
                            Some(&decl.name.text),
                        ) {
                            return Some(ResolvedCall {
                                signature,
                                method: true,
                            });
                        }
                    }
                }
            }
        }
        if let Some(binding_name) = receiver {
            if let Some(type_ref) = binding_type(ctx, binding_name, offset, 6) {
                if let Some(signature) = method_signature_for_type(ctx, &type_ref, parts.last()?) {
                    return Some(ResolvedCall {
                        signature,
                        method: true,
                    });
                }
            }
        }
        if let Some(module) = find_workspace_module(ctx, first) {
            if let Some(signature) = signature_for_module_member(ctx, &module, parts.last()?) {
                return Some(ResolvedCall {
                    signature,
                    method: false,
                });
            }
        }
        // A local binding used as a receiver: `order.ship(...)`.
        if let Some(type_ref) = binding_type(ctx, first, offset, 6) {
            if let Some(signature) = method_signature_for_type(ctx, &type_ref, parts.last()?) {
                return Some(ResolvedCall {
                    signature,
                    method: true,
                });
            }
        }
        return None;
    }

    let name = &parts[0];
    if let Some(receiver) = receiver {
        if let Some(type_ref) = binding_type(ctx, receiver, offset, 6) {
            if let Some(signature) = method_signature_for_type(ctx, &type_ref, name) {
                return Some(ResolvedCall {
                    signature,
                    method: true,
                });
            }
        }
        if let Some(signature) = method_signature(ctx, name) {
            return Some(ResolvedCall {
                signature,
                method: true,
            });
        }
    }
    if let Some(signature) = signature_for_name(ctx, name, true, None) {
        return Some(ResolvedCall {
            signature,
            method: false,
        });
    }
    if let Some(item) = ctx.host.builtins().lookup(name) {
        if let Some(signature) = signature_from_builtin(item) {
            return Some(ResolvedCall {
                signature,
                method: false,
            });
        }
    }
    None
}

fn signature_for_target_member(
    ctx: &FeatureContext<'_>,
    target: Target,
    rest: &[String],
) -> Option<Signature> {
    match target {
        Target::Module(module) => {
            let name = rest.last()?;
            signature_for_module_member(ctx, &module, name)
        }
        Target::Type { uri, mut path } => {
            path.extend(rest.iter().cloned());
            let analysis = if uri == ctx.analysis.uri {
                None
            } else {
                ctx.host.analyze(&uri)
            };
            let analysis_ref: &AnalyzedFile = analysis.as_deref().unwrap_or(ctx.analysis);
            let name = path.last()?;
            let decl = type_decl_at(analysis_ref, &path[..path.len() - 1])?;
            for item in &decl.members {
                if let ItemKind::Definition(definition) = &item.kind {
                    if definition
                        .simple_name()
                        .is_some_and(|def_name| def_name.text == *name)
                    {
                        return signature_from_definition(
                            ctx,
                            analysis_ref,
                            definition,
                            false,
                            Some(&decl.name.text),
                        );
                    }
                }
            }
            None
        }
        Target::BuiltinModule(module) => {
            let name = rest.last()?;
            let full = format!("{module}.{name}");
            ctx.host
                .builtins()
                .lookup(&full)
                .and_then(signature_from_builtin)
        }
        Target::Fields(_) => None,
    }
}

fn signature_for_module_member(
    ctx: &FeatureContext<'_>,
    module: &ModuleSummary,
    name: &str,
) -> Option<Signature> {
    if let Some(export) = module.export(name) {
        if let Some(signature) = signature_for_export(ctx, module, export) {
            return Some(signature);
        }
    }
    if let Some(analysis) = ctx.host.analyze(&module.uri) {
        if let Some(signature) = signature_for_name_in(&analysis, name, None) {
            return Some(signature);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Field and tag completion inside braces/brackets
// ---------------------------------------------------------------------------

fn record_field_completion(
    ctx: &FeatureContext<'_>,
    sig: &[SigToken<'_>],
    offset: u32,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) -> bool {
    let Some((brace_index, kind)) = open_bracket_before(sig, offset) else {
        return false;
    };
    if kind != TokenKind::LBrace {
        return false;
    }
    // A field name is expected after `{`, `,`, or at the start of a line.
    let line_start = ctx
        .analysis
        .source
        .line_start(ctx.analysis.source.line_of_offset(offset));
    let before = ctx
        .analysis
        .source
        .slice(TextRange::new(line_start, offset))
        .trim()
        .to_string();
    let without_prefix = before
        .trim_end_matches(|ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '!' | '$'));
    let without_prefix = without_prefix.trim_end();
    if !(without_prefix.is_empty()
        || without_prefix.ends_with(',')
        || without_prefix.ends_with('{'))
    {
        return false;
    }

    let (fields, container) = record_fields_for_brace(ctx, sig, brace_index);
    if fields.is_empty() {
        return false;
    }
    for field in fields {
        push_spec(out, seen, field_spec(&field));
    }
    let _ = container;
    true
}

fn record_fields_for_brace(
    ctx: &FeatureContext<'_>,
    sig: &[SigToken<'_>],
    brace_index: usize,
) -> (Vec<FieldInfo>, Option<String>) {
    // `Point.{ ... }`, or nominal construction `Point.({ ... })`.
    let index = brace_index;
    let dot_index = if index >= 2 && sig[index - 1].kind() == TokenKind::Dot {
        Some(index - 1)
    } else if index >= 3
        && sig[index - 1].kind() == TokenKind::LParen
        && sig[index - 2].kind() == TokenKind::Dot
    {
        Some(index - 2)
    } else {
        None
    };
    if let Some(dot_index) = dot_index {
        let parts = chain_before(sig, dot_index);
        if !parts.is_empty() {
            if let Some(target) = resolve_target(ctx, &parts, sig[brace_index].start()) {
                match target {
                    Target::Type { uri, path } => {
                        let analysis = if uri == ctx.analysis.uri {
                            None
                        } else {
                            ctx.host.analyze(&uri)
                        };
                        let analysis_ref: &AnalyzedFile =
                            analysis.as_deref().unwrap_or(ctx.analysis);
                        if let Some(decl) = type_decl_at(analysis_ref, &path) {
                            if let TypeKind::Record { fields, .. } = &decl.backing.kind {
                                return (
                                    fields
                                        .iter()
                                        .map(|field| field_info_from_type(analysis_ref, field))
                                        .collect(),
                                    Some(decl.name.text.clone()),
                                );
                            }
                        }
                    }
                    Target::Fields(fields) => return (fields, None),
                    _ => {}
                }
            }
            return (Vec::new(), None);
        }
    }

    let brace_range = sig[brace_index].token.range;

    // `pair : Pair(U64, Str)` followed by `pair = { ... }`.  The brace is in
    // the definition's value, not inside the annotation, so use the annotated
    // type of that value as the expected record shape.
    for definition in all_definitions_deep(ctx.analysis) {
        let Some(annotation) = &definition.annotation else {
            continue;
        };
        if definition.value.range.start != brace_range.start {
            continue;
        }
        if let Some(type_ref) = type_ref_from_annotation(ctx.analysis, annotation) {
            if let Some(fields) = fields_for_type_ref(ctx, &type_ref) {
                return (
                    fields,
                    definition.simple_name().map(|name| name.text.clone()),
                );
            }
        }
    }

    // `Counter := { value : U64 }` in a type declaration.
    for (_path, decl) in collect_type_nodes(ctx.analysis) {
        if decl.backing.range.contains_inclusive(brace_range.start) {
            if let TypeKind::Record { fields, .. } = &decl.backing.kind {
                return (
                    fields
                        .iter()
                        .map(|field| field_info_from_type(ctx.analysis, field))
                        .collect(),
                    Some(decl.name.text.clone()),
                );
            }
        }
        for item in &decl.members {
            if let ItemKind::TypeDecl(nested) = &item.kind {
                if nested.backing.range.contains_inclusive(brace_range.start) {
                    if let TypeKind::Record { fields, .. } = &nested.backing.kind {
                        return (
                            fields
                                .iter()
                                .map(|field| field_info_from_type(ctx.analysis, field))
                                .collect(),
                            Some(nested.name.text.clone()),
                        );
                    }
                }
            }
        }
    }

    // A record type annotation, e.g. `p : { x : U64, y : U64 }`.
    for (container, definition) in all_definitions(ctx.analysis) {
        if let Some(annotation) = &definition.annotation {
            if annotation.range.contains_inclusive(brace_range.start) {
                if let Some(type_ref) = type_ref_from_annotation(ctx.analysis, annotation) {
                    if let Some(fields) = fields_for_type_ref(ctx, &type_ref) {
                        return (fields, container);
                    }
                }
            }
        }
    }
    for item in &ctx.analysis.parsed.module.items {
        if let ItemKind::Annotation(annotation) = &item.kind {
            if annotation.ty.range.contains_inclusive(brace_range.start) {
                if let Some(type_ref) = type_ref_from_annotation(ctx.analysis, &annotation.ty) {
                    if let Some(fields) = fields_for_type_ref(ctx, &type_ref) {
                        return (fields, None);
                    }
                }
            }
        }
        if let ItemKind::TypeDecl(decl) = &item.kind {
            if decl.backing.range.contains_inclusive(brace_range.start) {
                if let TypeKind::Record { fields, .. } = &decl.backing.kind {
                    return (
                        fields
                            .iter()
                            .map(|field| field_info_from_type(ctx.analysis, field))
                            .collect(),
                        Some(decl.name.text.clone()),
                    );
                }
            }
        }
    }

    // Patterns: `|{ x, y }|` bound by an annotated parameter/record.
    let mut pattern_fields = Vec::new();
    for (container, definition) in all_definitions(ctx.analysis) {
        if definition
            .pattern
            .range
            .contains_inclusive(brace_range.start)
        {
            if let Some(annotation) = &definition.annotation {
                if let Some(type_ref) = type_ref_from_annotation(ctx.analysis, annotation) {
                    if let Some(fields) = fields_for_type_ref(ctx, &type_ref) {
                        pattern_fields = fields;
                    }
                }
            }
        }
        let _ = container;
    }
    (pattern_fields, None)
}

fn tag_completion(
    ctx: &FeatureContext<'_>,
    sig: &[SigToken<'_>],
    offset: u32,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) -> bool {
    let Some((bracket_index, kind)) = open_bracket_before(sig, offset) else {
        return false;
    };
    if kind != TokenKind::LBracket {
        return false;
    }
    let line_start = ctx
        .analysis
        .source
        .line_start(ctx.analysis.source.line_of_offset(offset));
    let before = ctx
        .analysis
        .source
        .slice(TextRange::new(line_start, offset))
        .trim()
        .to_string();
    let without_prefix = before
        .trim_end_matches(|ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '!' | '$'));
    let without_prefix = without_prefix.trim_end();
    if !(without_prefix.ends_with('[') || without_prefix.ends_with(',')) {
        return false;
    }
    let in_type_position = sig[..bracket_index]
        .iter()
        .rev()
        .take_while(|token| token.start() >= line_start)
        .any(|token| matches!(token.kind(), TokenKind::Colon | TokenKind::ColonEquals))
        || ctx.analysis.parsed.module.items.iter().any(|item| {
            matches!(
                &item.kind,
                ItemKind::TypeDecl(decl)
                    if decl.backing.range.contains_inclusive(sig[bracket_index].start())
            )
        });
    if !in_type_position {
        return false;
    }
    let mut added = false;
    for (_path, decl) in collect_type_nodes(ctx.analysis) {
        if let TypeKind::TagUnion { tags, .. } = &decl.backing.kind {
            for tag in tags {
                push_spec(out, seen, tag_specs_from_type(ctx.analysis, tag));
                added = true;
            }
        }
    }
    for (name, doc) in common_tags() {
        push_spec(
            out,
            seen,
            Spec::new(name, CompletionItemKind::ENUM_MEMBER, 1).doc(doc),
        );
        added = true;
    }
    added
}

fn common_tags() -> [(&'static str, &'static str); 6] {
    [
        ("Ok", "The success tag of a `Try`/`Result`."),
        ("Err", "The error tag of a `Try`/`Result`."),
        ("True", "Boolean true."),
        ("False", "Boolean false."),
        ("None", "The empty option tag."),
        ("Some", "A present option tag."),
    ]
}

// ---------------------------------------------------------------------------
// Header and general completion
// ---------------------------------------------------------------------------

fn in_header(ctx: &FeatureContext<'_>, offset: u32) -> bool {
    if let Some(header) = &ctx.analysis.parsed.module.header {
        if offset <= header.range.end {
            return true;
        }
        if ctx
            .analysis
            .parsed
            .module
            .items
            .iter()
            .all(|item| item.range.start > offset)
            && ctx
                .analysis
                .parsed
                .module
                .imports
                .iter()
                .all(|import| import.range.start > offset)
        {
            // Still typing the header, before the first item.
            return true;
        }
    }
    let text = ctx
        .analysis
        .source
        .slice(TextRange::new(0, offset.min(ctx.analysis.source.len())));
    let trimmed = text.trim_start();
    (trimmed.starts_with("app ")
        || trimmed.starts_with("package ")
        || trimmed.starts_with("platform "))
        && !text.contains("\nimport ")
        && !text.contains("\n\n")
}

fn header_completion(
    ctx: &FeatureContext<'_>,
    offset: u32,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) -> bool {
    if !in_header(ctx, offset) {
        return false;
    }
    let source = ctx.analysis.source.as_ref();
    let line_start = source.line_start(source.line_of_offset(offset));
    let before = source
        .slice(TextRange::new(line_start, offset))
        .trim()
        .to_string();
    let workspace = ctx.host.workspace();

    let mut shorthands = Vec::new();
    if let Some(header) = &ctx.analysis.parsed.module.header {
        for dependency in &header.dependencies {
            shorthands.push((
                dependency.shorthand.text.clone(),
                dependency.source.as_str().to_string(),
                dependency.is_platform,
            ));
        }
    }
    for package in &workspace.packages {
        if !shorthands
            .iter()
            .any(|(name, _, _)| *name == package.shorthand)
        {
            shorthands.push((
                package.shorthand.clone(),
                package.source_display(),
                matches!(package.kind, crate::analysis::PackageKind::Platform),
            ));
        }
    }
    if before.ends_with('{') || before.ends_with(',') || before.is_empty() {
        for (name, source, platform) in shorthands {
            let kind = if platform { "platform" } else { "package" };
            push_spec(
                out,
                seen,
                Spec::new(name.clone(), CompletionItemKind::MODULE, 2)
                    .detail(format!("{kind} · {source}"))
                    .insert(format!("{name}: ")),
            );
        }
        push_spec(
            out,
            seen,
            Spec::new("roc", CompletionItemKind::PROPERTY, 2)
                .detail("compiler release")
                .insert("roc: \"nightly\"")
                .snippet(),
        );
    }

    let wants_modules = before.contains("exposes [")
        || before.contains("provides [")
        || before.contains("hosted [")
        || before.contains("packages {");
    if wants_modules {
        for module in &workspace.modules {
            push_spec(
                out,
                seen,
                Spec::new(module.name.clone(), CompletionItemKind::MODULE, 2)
                    .detail(module.kind.as_str())
                    .data(json!({"kind": "module", "name": module.name})),
            );
        }
    }

    if ctx.config.completion.snippets {
        for (label, body, doc) in header_snippets() {
            push_spec(
                out,
                seen,
                Spec::new(label, CompletionItemKind::SNIPPET, 4)
                    .detail("header snippet")
                    .doc(doc)
                    .insert(body)
                    .snippet(),
            );
        }
    }
    true
}

fn header_snippets() -> [(&'static str, &'static str, &'static str); 3] {
    [
        (
            "app header",
            "app [main!] {\n\t${1:pf}: platform \"${2:url}\",\n\troc: \"${3:nightly}\",\n}",
            "Application header with a platform dependency.",
        ),
        (
            "package header",
            "package [${1:Module}] {\n\troc: \"${2:nightly}\",\n}",
            "Package header exposing modules.",
        ),
        (
            "platform header",
            "platform \"${1:name}\"\n\trequires ${2:{}}\n\texposes [${3:Module}]\n\tpackages { ${4} }\n\tprovides [${5:main!}]",
            "Platform header.",
        ),
    ]
}

fn general_completion(
    ctx: &FeatureContext<'_>,
    offset: u32,
    out: &mut Vec<Spec>,
    seen: &mut HashSet<String>,
) {
    // Locals win over everything else.
    for binding in ctx.analysis.visible_bindings(offset) {
        push_symbol_spec(ctx.analysis, binding, 0, out, seen);
    }
    // Top-level declarations and associated items.
    let shadowed: HashSet<String> = ctx
        .analysis
        .visible_bindings(offset)
        .into_iter()
        .map(|binding| binding.name.clone())
        .collect();
    let mut symbols = Vec::new();
    collect_symbols(&ctx.analysis.symbols, &mut symbols);
    for symbol in symbols {
        if shadowed.contains(&symbol.name) {
            continue;
        }
        push_symbol_spec(ctx.analysis, symbol, 0, out, seen);
    }
    for (path, decl) in collect_type_nodes(ctx.analysis) {
        if path.len() == 1 {
            push_spec(
                out,
                seen,
                Spec::new(decl.name.text.clone(), CompletionItemKind::STRUCT, 0)
                    .detail("type")
                    .doc(
                        decl.doc
                            .as_ref()
                            .map(|doc| doc.text.clone())
                            .unwrap_or_default(),
                    )
                    .data(json!({
                        "kind": "symbol",
                        "uri": ctx.analysis.uri.as_str(),
                        "name": decl.name.text,
                    })),
            );
        }
    }

    // Imports: qualifiers and exposed names.
    for import in &ctx.analysis.imports {
        push_spec(
            out,
            seen,
            Spec::new(import.qualifier.clone(), CompletionItemKind::MODULE, 2)
                .detail(format!("import {}", import.import.path.raw()))
                .insert(import.qualifier.clone())
                .data(json!({"kind": "module", "name": import.qualifier})),
        );
        for exposed in &import.exposing {
            let (detail, doc) = if let Some(module) = &import.target {
                match module.export(exposed) {
                    Some(export) => (
                        export
                            .detail
                            .clone()
                            .unwrap_or_else(|| kind_label(export.kind).to_string()),
                        export.doc.clone(),
                    ),
                    None => ("exposed item".to_string(), None),
                }
            } else {
                ("exposed item".to_string(), None)
            };
            push_spec(
                out,
                seen,
                Spec::new(exposed.clone(), CompletionItemKind::FUNCTION, 2)
                    .detail(detail)
                    .doc(doc.unwrap_or_default())
                    .data(json!({
                        "kind": "symbol",
                        "uri": import_target_uri(ctx, import).map(|uri| uri.to_string()),
                        "name": exposed,
                    })),
            );
        }
    }

    // Builtin modules/types.
    if ctx.config.completion.builtins {
        push_builtin_globals(ctx.host.builtins(), out, seen);
    }

    // Common tags.
    for (name, doc) in common_tags() {
        push_spec(
            out,
            seen,
            Spec::new(name, CompletionItemKind::ENUM_MEMBER, 3).doc(doc),
        );
    }

    // Snippets first, then keywords (so `if` etc. do not duplicate).
    if ctx.config.completion.snippets {
        for (label, body, doc) in expression_snippets() {
            push_spec(
                out,
                seen,
                Spec::new(label, CompletionItemKind::SNIPPET, 4)
                    .detail("snippet")
                    .doc(doc)
                    .insert(body)
                    .snippet(),
            );
        }
    }
    for keyword in KEYWORDS {
        push_spec(
            out,
            seen,
            Spec::new(*keyword, CompletionItemKind::KEYWORD, 4).detail("keyword"),
        );
    }
}

fn import_target_uri(
    ctx: &FeatureContext<'_>,
    import: &crate::analysis::ResolvedImport,
) -> Option<Url> {
    import_uri(ctx, import)
}

fn push_builtin_globals(builtins: &BuiltinIndex, out: &mut Vec<Spec>, seen: &mut HashSet<String>) {
    for module in builtins.modules() {
        let self_item = module.items.iter().find(|item| item.name == module.name);
        if let Some(item) = self_item {
            push_builtin_type(item, out, seen);
        } else {
            push_spec(
                out,
                seen,
                Spec::new(module.name.clone(), CompletionItemKind::MODULE, 3)
                    .detail("builtin module")
                    .doc(module.doc.clone().unwrap_or_default())
                    .data(json!({"kind": "builtin", "name": module.name})),
            );
        }

        // Numeric primitives live under `Num` in the generated API, but are
        // written unqualified in source (`I64`, `U64`, `F64`, `Dec`, ...).
        if module.name == "Num" {
            for item in &module.items {
                if item.kind == BuiltinKind::Type && is_global_num_type(&item.short_name) {
                    push_builtin_type(item, out, seen);
                }
            }
        }
    }
}

fn push_builtin_type(item: &BuiltinItem, out: &mut Vec<Spec>, seen: &mut HashSet<String>) {
    push_spec(
        out,
        seen,
        Spec::new(item.short_name.clone(), CompletionItemKind::STRUCT, 3)
            .detail(
                item.type_def
                    .clone()
                    .or_else(|| item.signature.clone())
                    .unwrap_or_else(|| "builtin type".to_string()),
            )
            .doc(item.doc.clone().unwrap_or_default())
            .data(json!({"kind": "builtin", "name": item.name})),
    );
}

fn is_global_num_type(short_name: &str) -> bool {
    matches!(
        short_name,
        "Dec"
            | "F32"
            | "F64"
            | "I8"
            | "I16"
            | "I32"
            | "I64"
            | "I128"
            | "U8"
            | "U16"
            | "U32"
            | "U64"
            | "U128"
            | "I8x16"
            | "I16x8"
            | "I32x4"
            | "I64x2"
            | "U8x16"
            | "U16x8"
            | "U32x4"
            | "U64x2"
            | "Numeral"
            | "Range"
    )
}

const KEYWORDS: &[&str] = &[
    "if", "else", "match", "for", "while", "return", "break", "crash", "dbg", "expect", "import",
    "as", "exposing", "var", "where", "and", "or",
];

fn expression_snippets() -> [(&'static str, &'static str, &'static str); 11] {
    [
        (
            "if",
            "if ${1:condition} {\n\t$0\n} else {\n\t$2\n}",
            "Conditional expression.",
        ),
        (
            "match",
            "match ${1:subject} {\n\t${2:pattern} => ${3:value}\n}",
            "Pattern match.",
        ),
        (
            "for",
            "for ${1:item} in ${2:list} {\n\t$0\n}",
            "Iterate over a list.",
        ),
        (
            "while",
            "while ${1:condition} {\n\t$0\n}",
            "Loop while a condition holds.",
        ),
        (
            "expect",
            "expect ${1:condition}",
            "Assertion checked by `roc test`.",
        ),
        ("lambda", "|${1:args}| {\n\t$0\n}", "Anonymous function."),
        ("annotation", ": ${1:Type}", "Type annotation."),
        ("import", "import ${1:Module}", "Import a module."),
        (
            "nominal type",
            "${1:Name} := { ${2:field} : ${3:U64} }",
            "Nominal record type.",
        ),
        ("record", "{ ${1:field}: ${2:value} }", "Record literal."),
        ("tag", "[${1:Tag}]", "Tag union."),
    ]
}

// ---------------------------------------------------------------------------
// Hover
// ---------------------------------------------------------------------------

pub fn hover(ctx: &FeatureContext<'_>, params: &HoverParams) -> Option<Hover> {
    let position = params.text_document_position_params.position;
    let offset = util::offset_at(ctx.analysis, position);
    let source = ctx.analysis.source.as_ref();

    if let Some(token) = ctx.analysis.token_at(offset) {
        if token.kind.is_comment() {
            return None;
        }
    }

    if let Some(hover) = hover_import(ctx, offset) {
        return Some(hover);
    }
    if let Some(hover) = hover_header_dependency(ctx, offset) {
        return Some(hover);
    }
    if let Some(hover) = hover_operator(ctx, offset) {
        return Some(hover);
    }
    if let Some(hover) = hover_keyword(ctx, offset) {
        return Some(hover);
    }

    let (parts, range) = qualified_chain_at(ctx, offset);
    if !parts.is_empty() {
        if let Some(hover) = hover_chain(ctx, &parts, offset) {
            let mut hover = hover;
            hover.range = Some(source.range(range));
            return Some(hover);
        }
    }

    if let Some(token) = ctx.analysis.token_at(offset) {
        if matches!(token.kind, TokenKind::LowerIdent | TokenKind::UpperIdent) {
            let text = token.text(source.text()).to_string();
            if let Some(hover) = hover_name(ctx, &text, offset) {
                let mut hover = hover;
                hover.range = Some(source.range(token.range));
                return Some(hover);
            }
        }
    }
    None
}

fn qualified_chain_at(ctx: &FeatureContext<'_>, offset: u32) -> (Vec<String>, TextRange) {
    let sig = significant(ctx.analysis);
    let Some(index) = token_index_at(&sig, offset) else {
        return (Vec::new(), TextRange::default());
    };
    let mut center = index;
    if sig[center].kind() == TokenKind::Dot {
        if center > 0 && sig[center - 1].is_name() {
            center -= 1;
        } else if center + 1 < sig.len() && sig[center + 1].is_name() {
            center += 1;
        } else {
            return (Vec::new(), TextRange::default());
        }
    }
    if !sig[center].is_name() {
        return (Vec::new(), TextRange::default());
    }
    let mut start = center;
    while start >= 2
        && sig[start - 1].kind() == TokenKind::Dot
        && sig[start - 2].is_name()
        && sig[start - 2].end() == sig[start - 1].start()
        && sig[start - 1].end() == sig[start].start()
    {
        start -= 2;
    }
    let mut end = center;
    while end + 2 < sig.len()
        && sig[end + 1].kind() == TokenKind::Dot
        && sig[end + 2].is_name()
        && sig[end].end() == sig[end + 1].start()
        && sig[end + 1].end() == sig[end + 2].start()
    {
        end += 2;
    }
    let mut parts = Vec::new();
    let mut index = start;
    while index <= end {
        if sig[index].is_name() {
            parts.push(sig[index].text.to_string());
        }
        index += 1;
    }
    let range = TextRange::new(sig[start].start(), sig[end].end());
    (parts, range)
}

fn hover_chain(ctx: &FeatureContext<'_>, parts: &[String], offset: u32) -> Option<Hover> {
    if parts.len() >= 2 {
        let full = parts.join(".");
        if let Some(item) = ctx.host.builtins().lookup(&full) {
            return Some(builtin_item_hover(item));
        }
    }

    if parts.len() == 1 {
        if let Some(hover) = hover_name(ctx, &parts[0], offset) {
            return Some(hover);
        }
    }

    // `Module.member`
    if let Some(import) = ctx
        .analysis
        .imports
        .iter()
        .find(|import| import.qualifier == parts[0])
    {
        if parts.len() >= 2 {
            if let Some(target) = import_target(ctx, import) {
                if let Some(hover) = hover_target_member(ctx, target, &parts[1..]) {
                    return Some(hover);
                }
            }
        }
        return Some(import_hover(ctx, import));
    }

    if let Some(module) = find_workspace_module(ctx, &parts[0]) {
        if parts.len() >= 2 {
            if let Some(hover) = hover_module_member(ctx, &module, &parts[1..]) {
                return Some(hover);
            }
        }
        return Some(module_hover(&module));
    }

    if let Some(decl) = type_decl_at(ctx.analysis, parts) {
        return Some(type_decl_hover(ctx.analysis, decl, None));
    }

    if let Some(hover) = hover_type_member(ctx, parts) {
        return Some(hover);
    }

    if let Some(definition_key) = ctx.analysis.definition_key_at(offset) {
        if let Some(hover) = hover_definition_key(ctx, definition_key) {
            return Some(hover);
        }
    }

    // A field of a typed local: `point.x`.
    if parts.len() >= 2 {
        if let Some(type_ref) = binding_type(ctx, &parts[0], offset, 6) {
            if let Some(fields) = fields_for_type_ref(ctx, &type_ref) {
                if let Some(field) = fields.iter().find(|field| field.name == parts[1]) {
                    return Some(field_hover(ctx, field, &parts[0]));
                }
            }
            if let Some(signature) = method_signature_for_type(ctx, &type_ref, &parts[1]) {
                let mut markdown = format!("```roc\n{}\n```", signature.display.trim());
                if let Some(doc) = &signature.doc {
                    markdown.push_str("\n\n---\n\n");
                    markdown.push_str(doc.trim());
                }
                add_metadata(
                    &mut markdown,
                    &[
                        ("kind", "method".to_string()),
                        ("receiver", parts[0].clone()),
                        ("module", ctx.analysis.module_name.clone()),
                    ],
                );
                return Some(Hover {
                    contents: HoverContents::Markup(util::markdown(markdown)),
                    range: None,
                });
            }
        }
    }

    None
}

fn hover_name(ctx: &FeatureContext<'_>, name: &str, offset: u32) -> Option<Hover> {
    if let Some(item) = ctx.host.builtins().lookup(name) {
        return Some(builtin_item_hover(item));
    }
    if let Some(module) = ctx.host.builtins().module(name) {
        let _ = module;
        return Some(builtin_module_hover(ctx.host.builtins(), name));
    }
    if let Some(definition_key) = ctx.analysis.definition_key_at(offset) {
        if let Some(hover) = hover_definition_key(ctx, definition_key) {
            return Some(hover);
        }
    }
    if let Some(symbol) = find_symbol(ctx.analysis, name, None) {
        if let Some(hover) = hover_symbol(ctx, ctx.analysis, symbol) {
            return Some(hover);
        }
    }
    // Exposed imports.
    for import in &ctx.analysis.imports {
        if import
            .exposing
            .iter()
            .any(|exposed| exposed.as_str() == name)
        {
            if let Some(module) = &import.target {
                if let Some(export) = module.export(name) {
                    if let Some(hover) = hover_export(ctx, module, export) {
                        return Some(hover);
                    }
                }
            }
            if let Some(uri) = import_uri(ctx, import) {
                if let Some(analysis) = ctx.host.analyze(&uri) {
                    if let Some(symbol) = find_symbol(&analysis, name, None) {
                        if let Some(hover) = hover_symbol(ctx, &analysis, symbol) {
                            return Some(hover);
                        }
                    }
                }
            }
        }
    }
    // Tags and their owning unions.
    if let Some(hover) = hover_tag(ctx, name) {
        return Some(hover);
    }
    None
}

fn hover_definition_key(ctx: &FeatureContext<'_>, key: &DefinitionKey) -> Option<Hover> {
    match key {
        DefinitionKey::Builtin { full_name } => {
            let item = ctx.host.builtins().lookup(full_name)?;
            Some(builtin_item_hover(item))
        }
        DefinitionKey::Module { uri } => {
            if let Some(module) = ctx.host.workspace().module_for_uri(uri) {
                Some(module_hover(module))
            } else {
                Some(Hover {
                    contents: HoverContents::Markup(util::markdown(format!(
                        "```roc\nimport {}\n```",
                        ctx.analysis.module_name
                    ))),
                    range: None,
                })
            }
        }
        DefinitionKey::Local { uri, def_start } => {
            let analysis = if uri == &ctx.analysis.uri {
                None
            } else {
                ctx.host.analyze(uri)
            };
            let analysis_ref: &AnalyzedFile = analysis.as_deref().unwrap_or(ctx.analysis);
            let symbol = analysis_ref.symbols.iter().find(|symbol| {
                symbol.name_range.start == *def_start
                    || symbol.name_range.contains_inclusive(*def_start)
            });
            let signature = analysis_ref.signature_for(key).cloned();
            let mut markdown = String::new();
            if let Some(signature) = &signature {
                markdown.push_str(&format!("```roc\n{}\n```", signature.display.trim()));
            } else if let Some(symbol) = symbol {
                if let Some(detail) = &symbol.detail {
                    markdown.push_str(&format!("```roc\n{}\n```", detail.trim()));
                }
            }
            let doc = signature
                .as_ref()
                .and_then(|signature| signature.doc.clone())
                .or_else(|| symbol.and_then(|symbol| symbol.doc.clone()));
            if let Some(doc) = doc {
                if !markdown.is_empty() {
                    markdown.push_str("\n\n---\n\n");
                }
                markdown.push_str(doc.trim());
            }
            if let Some(symbol) = symbol {
                add_metadata(
                    &mut markdown,
                    &[
                        ("kind", kind_label(symbol.kind).to_string()),
                        ("module", analysis_ref.module_name.clone()),
                    ],
                );
            }
            if markdown.trim().is_empty() {
                return None;
            }
            Some(Hover {
                contents: HoverContents::Markup(util::markdown(markdown)),
                range: None,
            })
        }
        DefinitionKey::TopLevel {
            uri,
            name,
            container,
        } => {
            let analysis = if uri == &ctx.analysis.uri {
                None
            } else {
                ctx.host.analyze(uri)
            };
            let analysis_ref: &AnalyzedFile = analysis.as_deref().unwrap_or(ctx.analysis);
            let symbol = find_symbol(analysis_ref, name, container.as_deref());
            let signature = signature_for_key(ctx, key)
                .or_else(|| signature_for_name_in(analysis_ref, name, container.as_deref()));
            let mut markdown = String::new();
            if let Some(signature) = &signature {
                markdown.push_str(&format!("```roc\n{}\n```", signature.display.trim()));
            } else if let Some(detail) = symbol.and_then(|symbol| symbol.detail.clone()) {
                markdown.push_str(&format!("```roc\n{}\n```", detail.trim()));
            }
            let doc = signature
                .as_ref()
                .and_then(|signature| signature.doc.clone())
                .or_else(|| symbol.and_then(|symbol| symbol.doc.clone()));
            if let Some(doc) = doc {
                if !markdown.is_empty() {
                    markdown.push_str("\n\n---\n\n");
                }
                markdown.push_str(doc.trim());
            }
            let kind = symbol
                .map(|symbol| kind_label(symbol.kind).to_string())
                .or_else(|| signature.as_ref().map(|_| "function".to_string()));
            if let Some(kind) = kind {
                add_metadata(
                    &mut markdown,
                    &[
                        ("kind", kind),
                        ("module", analysis_ref.module_name.clone()),
                        ("container", container.clone().unwrap_or_default()),
                    ],
                );
            }
            if markdown.trim().is_empty() {
                return None;
            }
            Some(Hover {
                contents: HoverContents::Markup(util::markdown(markdown)),
                range: None,
            })
        }
    }
}

fn hover_symbol(
    ctx: &FeatureContext<'_>,
    analysis: &AnalyzedFile,
    symbol: &SymbolInfo,
) -> Option<Hover> {
    let signature = analysis.signature_for(&symbol.key).cloned();
    let mut markdown = String::new();
    if let Some(signature) = &signature {
        markdown.push_str(&format!("```roc\n{}\n```", signature.display.trim()));
    } else if let Some(detail) = &symbol.detail {
        markdown.push_str(&format!("```roc\n{}\n```", detail.trim()));
    } else {
        markdown.push_str(&format!("```roc\n{}\n```", symbol.name));
    }
    let doc = signature
        .and_then(|signature| signature.doc)
        .or_else(|| symbol.doc.clone());
    if let Some(doc) = doc {
        markdown.push_str("\n\n---\n\n");
        markdown.push_str(doc.trim());
    }
    add_metadata(
        &mut markdown,
        &[
            ("kind", kind_label(symbol.kind).to_string()),
            ("module", analysis.module_name.clone()),
            ("container", symbol.container.clone().unwrap_or_default()),
        ],
    );
    let _ = ctx;
    Some(Hover {
        contents: HoverContents::Markup(util::markdown(markdown)),
        range: None,
    })
}

fn builtin_item_hover(item: &BuiltinItem) -> Hover {
    let code = item
        .signature
        .clone()
        .or_else(|| item.type_def.clone())
        .unwrap_or_else(|| item.name.clone());
    let mut markdown = format!("```roc\n{}\n```", code.trim());
    if let Some(doc) = &item.doc {
        if !doc.trim().is_empty() {
            markdown.push_str("\n\n---\n\n");
            markdown.push_str(doc.trim());
        }
    }
    add_metadata(
        &mut markdown,
        &[
            ("builtin", item.kind_string().to_string()),
            ("module", item.module.clone()),
            ("full name", format!("`{}`", item.name)),
        ],
    );
    Hover {
        contents: HoverContents::Markup(util::markdown(markdown)),
        range: None,
    }
}

fn builtin_module_hover(builtins: &BuiltinIndex, name: &str) -> Hover {
    let Some(module) = builtins.module(name) else {
        return Hover {
            contents: HoverContents::Markup(util::markdown(format!("**builtin** `{name}`"))),
            range: None,
        };
    };
    let mut markdown = format!("**builtin module** `{}`", module.name);
    if let Some(doc) = &module.doc {
        markdown.push_str("\n\n");
        markdown.push_str(doc.trim());
    }
    let members = module
        .items
        .iter()
        .filter(|item| item.name != module.name)
        .take(24)
        .map(|item| format!("`{}`", item.short_name))
        .collect::<Vec<_>>();
    if !members.is_empty() {
        markdown.push_str("\n\n**members:** ");
        markdown.push_str(&members.join(", "));
    }
    Hover {
        contents: HoverContents::Markup(util::markdown(markdown)),
        range: None,
    }
}

fn hover_export(
    ctx: &FeatureContext<'_>,
    module: &ModuleSummary,
    export: &Export,
) -> Option<Hover> {
    let signature = signature_for_export(ctx, module, export);
    let mut markdown = String::new();
    if let Some(signature) = &signature {
        markdown.push_str(&format!("```roc\n{}\n```", signature.display.trim()));
    } else if let Some(detail) = &export.detail {
        markdown.push_str(&format!("```roc\n{}\n```", detail.trim()));
    } else {
        markdown.push_str(&format!("```roc\n{}\n```", export.name));
    }
    let doc = export.doc.clone().or_else(|| {
        signature
            .as_ref()
            .and_then(|signature| signature.doc.clone())
    });
    if let Some(doc) = doc {
        markdown.push_str("\n\n---\n\n");
        markdown.push_str(doc.trim());
    }
    add_metadata(
        &mut markdown,
        &[
            ("kind", kind_label(export.kind).to_string()),
            ("module", module.name.clone()),
            ("package", module.package.clone().unwrap_or_default()),
        ],
    );
    Some(Hover {
        contents: HoverContents::Markup(util::markdown(markdown)),
        range: None,
    })
}

fn hover_target_member(ctx: &FeatureContext<'_>, target: Target, rest: &[String]) -> Option<Hover> {
    match target {
        Target::Module(module) => {
            if rest.len() == 1 {
                if let Some(export) = module.export(&rest[0]) {
                    return hover_export(ctx, &module, export);
                }
                if let Some(analysis) = ctx.host.analyze(&module.uri) {
                    if let Some(symbol) = find_symbol(&analysis, &rest[0], None) {
                        return hover_symbol(ctx, &analysis, symbol);
                    }
                }
            }
            None
        }
        Target::Type { uri, mut path } => {
            path.extend(rest.iter().cloned());
            hover_type_member_in_uri(ctx, &uri, &path)
        }
        Target::BuiltinModule(module) => {
            if rest.len() == 1 {
                let full = format!("{module}.{}", rest[0]);
                if let Some(item) = ctx.host.builtins().lookup(&full) {
                    return Some(builtin_item_hover(item));
                }
            }
            None
        }
        Target::Fields(fields) => {
            if rest.len() == 1 {
                fields
                    .iter()
                    .find(|field| field.name == rest[0])
                    .map(|field| field_hover(ctx, field, ""))
            } else {
                None
            }
        }
    }
}

fn hover_type_member(ctx: &FeatureContext<'_>, parts: &[String]) -> Option<Hover> {
    hover_type_member_in_uri(ctx, &ctx.analysis.uri, parts)
}

fn hover_type_member_in_uri(
    ctx: &FeatureContext<'_>,
    uri: &Url,
    parts: &[String],
) -> Option<Hover> {
    let analysis = if uri == &ctx.analysis.uri {
        None
    } else {
        ctx.host.analyze(uri)
    };
    let analysis_ref: &AnalyzedFile = analysis.as_deref().unwrap_or(ctx.analysis);
    if let Some(decl) = type_decl_at(analysis_ref, parts) {
        return Some(type_decl_hover(analysis_ref, decl, None));
    }
    if parts.len() >= 2 {
        let decl = type_decl_at(analysis_ref, &parts[..parts.len() - 1])?;
        let name = parts.last()?;
        for item in &decl.members {
            if let ItemKind::Definition(definition) = &item.kind {
                if definition
                    .simple_name()
                    .is_some_and(|def_name| def_name.text == *name)
                {
                    if let Some(signature) = signature_from_definition(
                        ctx,
                        analysis_ref,
                        definition,
                        false,
                        Some(&decl.name.text),
                    ) {
                        let mut markdown = format!("```roc\n{}\n```", signature.display.trim());
                        if let Some(doc) = &signature.doc {
                            markdown.push_str("\n\n---\n\n");
                            markdown.push_str(doc.trim());
                        }
                        add_metadata(
                            &mut markdown,
                            &[
                                ("kind", "method".to_string()),
                                ("type", decl.name.text.clone()),
                                ("module", analysis_ref.module_name.clone()),
                            ],
                        );
                        return Some(Hover {
                            contents: HoverContents::Markup(util::markdown(markdown)),
                            range: None,
                        });
                    }
                }
            }
        }
    }
    None
}

fn type_decl_hover(analysis: &AnalyzedFile, decl: &TypeDecl, path: Option<&[String]>) -> Hover {
    let mut markdown = format!("```roc\n{}\n```", type_text(analysis, &decl.backing));
    if let Some(doc) = &decl.doc {
        if !doc.text.trim().is_empty() {
            markdown.push_str("\n\n---\n\n");
            markdown.push_str(doc.text.trim());
        }
    }
    let mut extras = vec![
        ("kind", "type".to_string()),
        ("module", analysis.module_name.clone()),
    ];
    if let Some(path) = path {
        if path.len() > 1 {
            extras.push(("path", path.join(".")));
        }
    }
    match &decl.backing.kind {
        TypeKind::Record { fields, .. } => {
            extras.push((
                "fields",
                fields
                    .iter()
                    .map(|field| field.name.text.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }
        TypeKind::TagUnion { tags, .. } => {
            extras.push((
                "tags",
                tags.iter()
                    .map(|tag| tag.name.text.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }
        _ => {}
    }
    add_metadata(&mut markdown, &extras);
    Hover {
        contents: HoverContents::Markup(util::markdown(markdown)),
        range: None,
    }
}

fn module_hover(module: &ModuleSummary) -> Hover {
    let mut markdown = format!(
        "```roc\nimport {}\n```\n\n**{}** module",
        module.name,
        module.kind.as_str()
    );
    if let Some(doc) = &module.doc {
        markdown.push_str("\n\n");
        markdown.push_str(doc.trim());
    }
    let exports = module
        .exports
        .iter()
        .take(24)
        .map(|export| format!("`{}`", export.name))
        .collect::<Vec<_>>();
    if !exports.is_empty() {
        markdown.push_str("\n\n**exposes:** ");
        markdown.push_str(&exports.join(", "));
    }
    add_metadata(
        &mut markdown,
        &[
            ("path", module.path.display().to_string()),
            ("package", module.package.clone().unwrap_or_default()),
        ],
    );
    Hover {
        contents: HoverContents::Markup(util::markdown(markdown)),
        range: None,
    }
}

fn hover_module_member(
    ctx: &FeatureContext<'_>,
    module: &ModuleSummary,
    rest: &[String],
) -> Option<Hover> {
    if rest.len() == 1 {
        if let Some(export) = module.export(&rest[0]) {
            return hover_export(ctx, module, export);
        }
        if let Some(analysis) = ctx.host.analyze(&module.uri) {
            if let Some(symbol) = find_symbol(&analysis, &rest[0], None) {
                return hover_symbol(ctx, &analysis, symbol);
            }
            let mut candidate = vec![module
                .name
                .rsplit('/')
                .next()
                .unwrap_or(&module.name)
                .to_string()];
            candidate.extend(rest.iter().cloned());
            if let Some(decl) = type_decl_at(&analysis, &candidate) {
                return Some(type_decl_hover(&analysis, decl, Some(&candidate)));
            }
        }
    }
    hover_type_member_in_uri(ctx, &module.uri, rest)
}

fn field_hover(ctx: &FeatureContext<'_>, field: &FieldInfo, owner: &str) -> Hover {
    let mut markdown = format!("```roc\n{}", field.name);
    if let Some(ty) = &field.ty {
        markdown.push_str(&format!(" : {ty}"));
    } else {
        markdown.push_str(" : _");
    }
    if field.optional {
        markdown.push_str("  # optional");
    }
    markdown.push_str("\n```");
    if let Some(default) = &field.default {
        markdown.push_str(&format!("\n\nDefault: `{default}`"));
    }
    add_metadata(
        &mut markdown,
        &[
            ("kind", "field".to_string()),
            ("record", owner.to_string()),
            ("module", ctx.analysis.module_name.clone()),
        ],
    );
    Hover {
        contents: HoverContents::Markup(util::markdown(markdown)),
        range: None,
    }
}

fn hover_tag(ctx: &FeatureContext<'_>, name: &str) -> Option<Hover> {
    for (path, decl) in collect_type_nodes(ctx.analysis) {
        if let TypeKind::TagUnion { tags, .. } = &decl.backing.kind {
            if let Some(tag) = tags.iter().find(|tag| tag.name.text == name) {
                let mut markdown = format!("```roc\n{}\n```", tag_spec_detail(ctx.analysis, tag));
                markdown.push_str(&format!("\n\nTag `{name}` of `{}`", path.join(".")));
                if let Some(doc) = &decl.doc {
                    if !doc.text.trim().is_empty() {
                        markdown.push_str("\n\n");
                        markdown.push_str(doc.text.trim());
                    }
                }
                add_metadata(
                    &mut markdown,
                    &[
                        ("kind", "tag".to_string()),
                        ("union", path.join(".")),
                        ("module", ctx.analysis.module_name.clone()),
                    ],
                );
                return Some(Hover {
                    contents: HoverContents::Markup(util::markdown(markdown)),
                    range: None,
                });
            }
        }
    }
    let literal = match name {
        "Ok" => Some(("Ok(value)", "Success case of `Try(a, e)` / `Result`.")),
        "Err" => Some(("Err(error)", "Error case of `Try(a, e)` / `Result`.")),
        "True" => Some(("True", "Boolean true.")),
        "False" => Some(("False", "Boolean false.")),
        "None" => Some(("None", "Empty optional value.")),
        "Some" => Some(("Some(value)", "Present optional value.")),
        _ => None,
    };
    literal.map(|(code, doc)| Hover {
        contents: HoverContents::Markup(util::markdown(format!("```roc\n{code}\n```\n\n{doc}"))),
        range: None,
    })
}

fn tag_spec_detail(analysis: &AnalyzedFile, tag: &TypeTag) -> String {
    if tag.payloads.is_empty() {
        tag.name.text.clone()
    } else {
        format!(
            "{}({})",
            tag.name.text,
            tag.payloads
                .iter()
                .map(|payload| type_text(analysis, payload))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn hover_import(ctx: &FeatureContext<'_>, offset: u32) -> Option<Hover> {
    for resolved in &ctx.analysis.imports {
        let import = &resolved.import;
        let path_range = import.path.range();
        let in_path = path_range.contains_inclusive(offset);
        let in_alias = import
            .alias
            .as_ref()
            .is_some_and(|alias| alias.range.contains_inclusive(offset));
        let exposing = import
            .exposing
            .iter()
            .find(|name| name.range.contains_inclusive(offset));
        if !(in_path || in_alias || exposing.is_some() || import.range.contains_inclusive(offset)) {
            continue;
        }
        if let Some(name) = exposing {
            if let Some(module) = &resolved.target {
                if let Some(export) = module.export(&name.text) {
                    if let Some(mut hover) = hover_export(ctx, module, export) {
                        hover.range = Some(ctx.analysis.source.range(name.range));
                        return Some(hover);
                    }
                }
            }
            if let Some(uri) = import_uri(ctx, resolved) {
                if let Some(analysis) = ctx.host.analyze(&uri) {
                    if let Some(symbol) = find_symbol(&analysis, &name.text, None) {
                        if let Some(mut hover) = hover_symbol(ctx, &analysis, symbol) {
                            hover.range = Some(ctx.analysis.source.range(name.range));
                            return Some(hover);
                        }
                    }
                }
            }
        }
        let mut hover = import_hover(ctx, resolved);
        hover.range = Some(ctx.analysis.source.range(if in_alias {
            import
                .alias
                .as_ref()
                .map(|alias| alias.range)
                .unwrap_or(path_range)
        } else {
            path_range
        }));
        return Some(hover);
    }
    None
}

fn import_hover(ctx: &FeatureContext<'_>, resolved: &crate::analysis::ResolvedImport) -> Hover {
    let import = &resolved.import;
    let mut code = format!("import {}", import.path.raw());
    if let Some(alias) = &import.alias {
        code.push_str(&format!(" as {}", alias.text));
    }
    if let Some(ty) = &import.ty {
        code.push_str(&format!(" : {}", type_text(ctx.analysis, ty)));
    }
    let mut markdown = format!("```roc\n{code}\n```");
    let mut extras = vec![("qualifier", resolved.qualifier.clone())];
    if let Some(module) = &resolved.target {
        extras.push(("module", module.name.clone()));
        extras.push(("kind", module.kind.as_str().to_string()));
        extras.push(("path", module.path.display().to_string()));
        if let Some(package) = &module.package {
            extras.push(("package", package.clone()));
        }
    } else if let Some(uri) = import_uri(ctx, resolved) {
        if let Ok(path) = uri.to_file_path() {
            extras.push(("path", path.display().to_string()));
        }
    }
    if let ImportPath::File { path, .. } = &import.path {
        extras.push(("file import", path.clone()));
    }
    if !resolved.exposing.is_empty() {
        extras.push(("exposing", resolved.exposing.join(", ")));
    } else if let Some(module) = &resolved.target {
        let names = module
            .exports
            .iter()
            .take(24)
            .map(|export| export.name.clone())
            .collect::<Vec<_>>();
        if !names.is_empty() {
            extras.push(("exposes", names.join(", ")));
        }
    }
    if let Some(error) = &resolved.error {
        extras.push(("error", error.clone()));
    }

    // Package details are the main import-inspection surface.
    if let Some(package) = resolved
        .target
        .as_ref()
        .and_then(|module| module.package.clone())
        .or_else(|| import.path.raw().split('.').next().map(str::to_string))
    {
        if let Some(package) = ctx.host.workspace().package(&package) {
            markdown.push_str("\n\n");
            markdown.push_str(&package_markdown(ctx, package));
        } else if let Some(dependency) = header_dependency(ctx, &package) {
            markdown.push_str("\n\n");
            markdown.push_str(&dependency_markdown(
                ctx,
                &dependency.shorthand.text,
                dependency.source.as_str(),
            ));
        }
    }
    add_metadata(&mut markdown, &extras);
    Hover {
        contents: HoverContents::Markup(util::markdown(markdown)),
        range: None,
    }
}

fn hover_header_dependency(ctx: &FeatureContext<'_>, offset: u32) -> Option<Hover> {
    let header = ctx.analysis.parsed.module.header.as_ref()?;
    for dependency in &header.dependencies {
        if dependency.range.contains_inclusive(offset)
            || dependency.source_range.contains_inclusive(offset)
            || dependency.shorthand.range.contains_inclusive(offset)
        {
            let mut code = format!(
                "{}: {}\"{}\"",
                dependency.shorthand.text,
                if dependency.is_platform {
                    "platform "
                } else {
                    ""
                },
                dependency.source.as_str()
            );
            code = code.replace("\"\"", "");
            let mut markdown = format!("```roc\n{code}\n```\n\n");
            markdown.push_str(&dependency_markdown(
                ctx,
                &dependency.shorthand.text,
                dependency.source.as_str(),
            ));
            return Some(Hover {
                contents: HoverContents::Markup(util::markdown(markdown)),
                range: Some(ctx.analysis.source.range(dependency.range)),
            });
        }
    }
    None
}

fn header_dependency<'a>(
    ctx: &FeatureContext<'a>,
    shorthand: &str,
) -> Option<&'a crate::syntax::ast::Dependency> {
    ctx.analysis
        .parsed
        .module
        .header
        .as_ref()?
        .dependencies
        .iter()
        .find(|dependency| dependency.shorthand.text == shorthand)
}

fn dependency_markdown(ctx: &FeatureContext<'_>, shorthand: &str, source: &str) -> String {
    let workspace = ctx.host.workspace();
    if let Some(package) = workspace.package(shorthand) {
        return package_markdown(ctx, package);
    }
    let declaring = ctx.analysis.source.path();
    let dependency_source = crate::syntax::ast::DependencySource::Url(source.to_string());
    let (root, error) = match (source.starts_with("http"), declaring.as_deref()) {
        (true, Some(declaring)) => resolve_dependency_root(declaring, &dependency_source),
        (false, Some(declaring)) => resolve_dependency_root(
            declaring,
            &crate::syntax::ast::DependencySource::Path(source.to_string()),
        ),
        _ => (None, None),
    };
    let version = parse_url_version(source);
    let mut text = format!("**package:** `{shorthand}`");
    if let Some(version) = version {
        text.push_str(&format!("\n\n**version:** {version}"));
    } else if source.starts_with("nightly") || source.starts_with("http") {
        // no version
    }
    text.push_str(&format!("\n\n**source:** `{source}`"));
    if let Some(root) = &root {
        text.push_str(&format!("\n\n**path:** `{}`", root.display()));
        let status = if root.is_dir() {
            "downloaded"
        } else {
            "not downloaded"
        };
        text.push_str(&format!("\n\n**cache:** {status}"));
    }
    if let Some(error) = error {
        text.push_str(&format!("\n\n**status:** {error}"));
    }
    text
}

fn package_markdown(ctx: &FeatureContext<'_>, package: &PackageInfo) -> String {
    let mut text = format!(
        "**package:** `{}` ({})",
        package.display_name(),
        package.kind.as_str()
    );
    if let Some(version) = &package.version {
        text.push_str(&format!("\n\n**version:** {version}"));
    }
    text.push_str(&format!("\n\n**source:** `{}`", package.source_display()));
    if let Some(root) = &package.root {
        text.push_str(&format!("\n\n**path:** `{}`", root.display()));
        let status = if root.is_dir() {
            "downloaded"
        } else {
            "not downloaded"
        };
        text.push_str(&format!("\n\n**cache:** {status}"));
    }
    if let Some(root_file) = &package.root_file {
        text.push_str(&format!("\n\n**root file:** `{}`", root_file.display()));
    }
    if !package.exposes.is_empty() {
        text.push_str(&format!("\n\n**exposes:** {}", package.exposes.join(", ")));
    }
    if !package.modules.is_empty() {
        text.push_str(&format!("\n\n**modules:** {}", package.modules.join(", ")));
    }
    if !package.dependencies.is_empty() {
        let deps = package
            .dependencies
            .iter()
            .map(|dependency| {
                format!(
                    "`{}` → `{}`",
                    dependency.shorthand,
                    dependency.source.as_str()
                )
            })
            .collect::<Vec<_>>();
        text.push_str(&format!("\n\n**dependencies:** {}", deps.join(", ")));
    }
    if let Some(error) = &package.error {
        text.push_str(&format!("\n\n**status:** {error}"));
    }
    let _ = ctx;
    text
}

fn hover_operator(ctx: &FeatureContext<'_>, offset: u32) -> Option<Hover> {
    let token = ctx.analysis.token_at(offset)?;
    if !matches!(
        token.kind,
        TokenKind::Operator
            | TokenKind::Pipe
            | TokenKind::Question
            | TokenKind::OptionalQuestion
            | TokenKind::Bang
    ) {
        return None;
    }
    let text = token.text(ctx.analysis.source.text());
    let (code, description) = match text {
        "+" => ("a + b", "Addition; desugars to `a.plus(b)`."),
        "-" => ("a - b", "Subtraction; desugars to `a.minus(b)`."),
        "*" => ("a * b", "Multiplication; desugars to `a.times(b)`."),
        "/" => ("a / b", "Fractional division; desugars to `a.div_by(b)`."),
        "//" => ("a // b", "Integer division, truncating toward zero."),
        "%" => ("a % b", "Remainder; desugars to `a.rem(b)`."),
        "==" => ("a == b", "Structural equality."),
        "!=" => ("a != b", "Structural inequality."),
        "<" | ">" | "<=" | ">=" => (text, "Structural ordering comparison."),
        "|>" => (
            "value |> function",
            "Pipeline: passes the left value as the **first** argument of the call on the right.",
        ),
        "??" => (
            "result ?? default",
            "Default an `Err` (or `None`) to a fallback value, producing a non-`Result` value.",
        ),
        "?" => (
            "result?",
            "Postfix error propagation: returns early with `Err` if the value is `Err`.",
        ),
        "!" => (
            "!value / function!",
            "Unary boolean negation, or the effectful-function suffix on a name.",
        ),
        "&" => ("a & b", "Boolean and (bitwise on integers)."),
        "|" => (
            "a | b",
            "Boolean or (bitwise on integers); also separates match alternatives.",
        ),
        "^" => ("a ^ b", "Bitwise exclusive or."),
        "<<" => ("a << b", "Bitwise shift left."),
        ">>" => ("a >> b", "Bitwise shift right."),
        _ => return None,
    };
    Some(Hover {
        contents: HoverContents::Markup(util::markdown(format!(
            "```roc\n{code}\n```\n\n{description}"
        ))),
        range: Some(ctx.analysis.source.range(token.range)),
    })
}

fn hover_keyword(ctx: &FeatureContext<'_>, offset: u32) -> Option<Hover> {
    let token = ctx.analysis.token_at(offset)?;
    if token.kind != TokenKind::Keyword {
        return None;
    }
    let keyword = token.keyword?;
    let description = match keyword {
        crate::syntax::Keyword::Import => "Import a module or a compile-time file.",
        crate::syntax::Keyword::Exposing => "List the items an import makes visible unqualified.",
        crate::syntax::Keyword::As => "Alias the imported module or file value.",
        crate::syntax::Keyword::Expect => "Assertion evaluated by `roc test`.",
        crate::syntax::Keyword::Dbg => "Print a value for debugging and return it unchanged.",
        crate::syntax::Keyword::Crash => "Abort with an error message.",
        crate::syntax::Keyword::Where => "Constrain type variables with abilities.",
        crate::syntax::Keyword::And | crate::syntax::Keyword::Or => "Boolean operator.",
        crate::syntax::Keyword::If | crate::syntax::Keyword::Else => "Conditional expression.",
        crate::syntax::Keyword::Match => "Pattern match expression.",
        crate::syntax::Keyword::For => "Iterate over a list.",
        crate::syntax::Keyword::While => "Loop while a condition holds.",
        _ => return None,
    };
    Some(Hover {
        contents: HoverContents::Markup(util::markdown(format!(
            "**`{}`**\n\n{description}",
            keyword.as_str()
        ))),
        range: Some(ctx.analysis.source.range(token.range)),
    })
}

fn add_metadata(markdown: &mut String, extras: &[(&str, String)]) {
    let extras = extras
        .iter()
        .filter(|(_, value)| !value.trim().is_empty())
        .collect::<Vec<_>>();
    if extras.is_empty() {
        return;
    }
    markdown.push_str("\n\n");
    for (label, value) in extras {
        markdown.push_str(&format!("\n**{label}:** {value}"));
    }
}

// ---------------------------------------------------------------------------
// Signature help
// ---------------------------------------------------------------------------

pub fn signature_help(
    ctx: &FeatureContext<'_>,
    params: &SignatureHelpParams,
) -> Option<SignatureHelp> {
    let position = params.text_document_position_params.position;
    let offset = util::offset_at(ctx.analysis, position);
    if let Some(token) = ctx.analysis.token_at(offset) {
        if token.kind.is_comment() {
            return None;
        }
    }
    let sig = significant(ctx.analysis);
    let (open_index, kind) = open_bracket_before(&sig, offset)?;
    if kind != TokenKind::LParen {
        return None;
    }
    let commas = top_level_commas(&sig, open_index, offset);
    let (parts, start_index) = call_chain_before(&sig, open_index)?;
    let pipe = start_index > 0 && is_pipe(sig[start_index - 1]);
    let previous = previous_token(&sig, start_index);
    let has_receiver = parts.len() == 1
        && previous.is_some_and(|token| is_expression_end(token) && !is_pipe(token));
    let receiver = if has_receiver {
        previous.and_then(|token| token.is_name().then_some(token.text))
    } else {
        None
    };
    let resolved = resolve_chain_signature(ctx, &parts, receiver, offset)?;
    let signature = resolved.signature;
    let method = has_receiver || resolved.method;
    let hide_receiver = usize::from(method);
    let offset_params = usize::from(method || pipe);
    let information = signature_information(&signature, hide_receiver, offset_params, commas);
    let active = information.active_parameter;
    Some(SignatureHelp {
        signatures: vec![information],
        active_signature: Some(0),
        active_parameter: active,
    })
}

fn call_chain_before(sig: &[SigToken<'_>], open_index: usize) -> Option<(Vec<String>, usize)> {
    if open_index == 0 {
        return None;
    }
    let mut index = open_index;
    let mut parts = Vec::new();
    while index >= 1 {
        let name = sig[index - 1];
        if !name.is_name() {
            break;
        }
        parts.push(name.text.to_string());
        if index >= 2 && sig[index - 2].kind() == TokenKind::Dot {
            index -= 2;
        } else {
            index -= 1;
            break;
        }
    }
    if parts.is_empty() {
        return None;
    }
    parts.reverse();
    Some((parts, index))
}

// ---------------------------------------------------------------------------
// Inlay hints
// ---------------------------------------------------------------------------

pub fn inlay_hints(ctx: &FeatureContext<'_>, params: &InlayHintParams) -> Option<Vec<InlayHint>> {
    let config = &ctx.config.inlay_hints;
    if !config.enabled {
        return None;
    }
    let range = ctx.analysis.source.text_range(params.range);
    let mut sink = HintSink::new(ctx, range);
    visit_items_hints(ctx, &ctx.analysis.parsed.module.items, false, &mut sink);
    Some(sink.hints)
}

struct HintSink<'a, 'b> {
    ctx: &'a FeatureContext<'b>,
    range: TextRange,
    hints: Vec<InlayHint>,
    seen: HashSet<(u32, String)>,
}

impl<'a, 'b> HintSink<'a, 'b> {
    fn new(ctx: &'a FeatureContext<'b>, range: TextRange) -> Self {
        Self {
            ctx,
            range,
            hints: Vec::new(),
            seen: HashSet::new(),
        }
    }

    fn add(
        &mut self,
        offset: u32,
        label: String,
        kind: InlayHintKind,
        tooltip: String,
        padding_left: bool,
    ) {
        let offset = offset.min(self.ctx.analysis.source.len());
        if offset < self.range.start || offset > self.range.end {
            return;
        }
        if label.trim().is_empty() {
            return;
        }
        if !self.seen.insert((offset, label.clone())) {
            return;
        }
        self.hints.push(InlayHint {
            position: self.ctx.analysis.source.position_at(offset),
            label: InlayHintLabel::String(label),
            kind: Some(kind),
            text_edits: None,
            tooltip: Some(InlayHintTooltip::String(tooltip)),
            padding_left: Some(padding_left),
            padding_right: None,
            data: None,
        });
    }
}

fn visit_items_hints(
    ctx: &FeatureContext<'_>,
    items: &[Item],
    in_block: bool,
    sink: &mut HintSink<'_, '_>,
) {
    for item in items {
        match &item.kind {
            ItemKind::Definition(definition) => {
                visit_definition_hints(ctx, definition, in_block, sink)
            }
            ItemKind::TypeDecl(decl) => {
                visit_items_hints(ctx, &decl.members, false, sink);
            }
            ItemKind::Expect(expect) => visit_expr_hints(ctx, &expect.expr, None, sink),
            ItemKind::Annotation(annotation) => {
                let _ = annotation;
            }
            ItemKind::WhereAlias(_) => {}
            ItemKind::Error => {}
        }
    }
}

fn visit_definition_hints(
    ctx: &FeatureContext<'_>,
    definition: &Definition,
    in_block: bool,
    sink: &mut HintSink<'_, '_>,
) {
    let config = &ctx.config.inlay_hints;
    if let Some(annotation) = &definition.annotation {
        match &annotation.kind {
            TypeKind::Function { ret, .. } => {
                if config.return_types {
                    let line = ctx
                        .analysis
                        .source
                        .line_of_offset(definition.equals_range.start);
                    let line_end = ctx.analysis.source.line_end(line);
                    sink.add(
                        line_end,
                        format!(": {}", type_text(ctx.analysis, ret)),
                        InlayHintKind::TYPE,
                        "return type".to_string(),
                        true,
                    );
                }
            }
            _ => {
                if in_block && config.binding_types {
                    if let Some(name) = definition.simple_name() {
                        sink.add(
                            name.range.end,
                            format!(": {}", type_text(ctx.analysis, annotation)),
                            InlayHintKind::TYPE,
                            format!("declared type of `{}`", name.text),
                            false,
                        );
                    }
                }
            }
        }
    }
    visit_expr_hints(ctx, &definition.value, definition.annotation.as_ref(), sink);
}

fn visit_expr_hints(
    ctx: &FeatureContext<'_>,
    expr: &Expr,
    expected: Option<&TypeExpr>,
    sink: &mut HintSink<'_, '_>,
) {
    if !expr.range.intersects(sink.range) {
        return;
    }
    match &expr.kind {
        ExprKind::Call { callee, args } => {
            if let Some(resolved) = resolve_call_signature(ctx, callee, false) {
                let signature = resolved.signature;
                let pipe = pipe_before_expr(ctx, callee);
                let offset = usize::from(resolved.method || pipe);
                add_parameter_hints(ctx, sink, &signature, args, offset, None);
                visit_call_args(ctx, sink, &signature, args, offset);
            } else {
                for arg in args {
                    visit_expr_hints(ctx, arg, None, sink);
                }
            }
            visit_expr_hints(ctx, callee, None, sink);
        }
        ExprKind::MethodCall {
            receiver,
            method,
            args,
        } => {
            let receiver_type = infer_expr_type(ctx, receiver, 6).unwrap_or(TypeRef::Unknown);
            if let Some(signature) = method_signature_for_type(ctx, &receiver_type, &method.text)
                .or_else(|| method_signature(ctx, &method.text))
            {
                add_parameter_hints(ctx, sink, &signature, args, 1, Some(receiver));
                visit_call_args(ctx, sink, &signature, args, 1);
            } else {
                for arg in args {
                    visit_expr_hints(ctx, arg, None, sink);
                }
            }
            visit_expr_hints(ctx, receiver, None, sink);
        }
        ExprKind::Lambda { params, body } => {
            if let Some(TypeExpr {
                kind: TypeKind::Function { args, .. },
                ..
            }) = expected
            {
                if ctx.config.inlay_hints.parameter_types {
                    for (index, pattern) in params.iter().enumerate() {
                        if let (Some(ty), Some(name)) = (args.get(index), pattern_name(pattern)) {
                            sink.add(
                                name.range.end,
                                format!(": {}", type_text(ctx.analysis, ty)),
                                InlayHintKind::TYPE,
                                format!("type of parameter `{}`", name.text),
                                false,
                            );
                        }
                    }
                }
            }
            visit_expr_hints(ctx, body, None, sink);
        }
        ExprKind::Block { stmts, tail } => {
            for stmt in stmts {
                match &stmt.kind {
                    StmtKind::Definition(definition) => {
                        visit_definition_hints(ctx, definition, true, sink)
                    }
                    StmtKind::TypeDecl(_) => {}
                    StmtKind::Assignment { value, .. } => visit_expr_hints(ctx, value, None, sink),
                    StmtKind::Db(value) | StmtKind::Expect(value) | StmtKind::Crash(value) => {
                        visit_expr_hints(ctx, value, None, sink)
                    }
                    StmtKind::Return(Some(value)) => visit_expr_hints(ctx, value, None, sink),
                    StmtKind::Expr(value) => visit_expr_hints(ctx, value, None, sink),
                    StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
                }
            }
            if let Some(tail) = tail {
                visit_expr_hints(ctx, tail, None, sink);
            }
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            visit_expr_hints(ctx, condition, None, sink);
            visit_expr_hints(ctx, then_branch, None, sink);
            visit_expr_hints(ctx, else_branch, None, sink);
        }
        ExprKind::Match { subject, branches } => {
            visit_expr_hints(ctx, subject, None, sink);
            for branch in branches {
                if let Some(guard) = &branch.guard {
                    visit_expr_hints(ctx, guard, None, sink);
                }
                visit_expr_hints(ctx, &branch.body, None, sink);
            }
        }
        ExprKind::Record { fields, update } => {
            for field in fields {
                if let Some(value) = &field.value {
                    visit_expr_hints(ctx, value, None, sink);
                }
            }
            if let Some(update) = update {
                visit_expr_hints(ctx, update, None, sink);
            }
        }
        ExprKind::List(items) | ExprKind::Tuple(items) => {
            for item in items {
                visit_expr_hints(ctx, item, None, sink);
            }
        }
        ExprKind::Tag { args, .. } => {
            for arg in args {
                visit_expr_hints(ctx, arg, None, sink);
            }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            visit_expr_hints(ctx, lhs, None, sink);
            visit_expr_hints(ctx, rhs, None, sink);
        }
        ExprKind::Unary { expr, .. }
        | ExprKind::Paren(expr)
        | ExprKind::Db(expr)
        | ExprKind::Crash(expr) => visit_expr_hints(ctx, expr, None, sink),
        ExprKind::For { iterable, body, .. } => {
            visit_expr_hints(ctx, iterable, None, sink);
            visit_expr_hints(ctx, body, None, sink);
        }
        ExprKind::While { condition, body } => {
            visit_expr_hints(ctx, condition, None, sink);
            visit_expr_hints(ctx, body, None, sink);
        }
        ExprKind::Return(Some(value)) => visit_expr_hints(ctx, value, None, sink),
        ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => {
            visit_expr_hints(ctx, base, None, sink)
        }
        ExprKind::Nominal { value, .. } => visit_expr_hints(ctx, value, None, sink),
        ExprKind::String(parts) | ExprKind::MultilineString(parts) => {
            for part in parts {
                if let crate::syntax::ast::StrPartKind::Interpolation(inner) = &part.kind {
                    visit_expr_hints(ctx, inner, None, sink);
                }
            }
        }
        ExprKind::Number(_)
        | ExprKind::Char(_)
        | ExprKind::Lookup { .. }
        | ExprKind::Return(None)
        | ExprKind::Break
        | ExprKind::Continue
        | ExprKind::Error => {}
    }
}

fn visit_call_args(
    ctx: &FeatureContext<'_>,
    sink: &mut HintSink<'_, '_>,
    signature: &Signature,
    args: &[Expr],
    offset: usize,
) {
    for (index, arg) in args.iter().enumerate() {
        let Some(param) = signature.params.get(index + offset) else {
            break;
        };
        let expected_functions = param.ty.as_deref().and_then(parse_function_type_text);
        if let Some((arg_types, _ret)) = expected_functions {
            if let ExprKind::Lambda { params, body } = &arg.kind {
                if ctx.config.inlay_hints.parameter_types {
                    for (type_index, pattern) in params.iter().enumerate() {
                        if let (Some(ty), Some(name)) =
                            (arg_types.get(type_index), pattern_name(pattern))
                        {
                            sink.add(
                                name.range.end,
                                format!(": {ty}"),
                                InlayHintKind::TYPE,
                                format!("type of parameter `{}`", name.text),
                                false,
                            );
                        }
                    }
                }
                visit_expr_hints(ctx, body, None, sink);
                continue;
            }
        }
        visit_expr_hints(ctx, arg, None, sink);
    }
}

fn add_parameter_hints(
    ctx: &FeatureContext<'_>,
    sink: &mut HintSink<'_, '_>,
    signature: &Signature,
    args: &[Expr],
    offset: usize,
    _receiver: Option<&Expr>,
) {
    if !ctx.config.inlay_hints.parameter_names {
        return;
    }
    for (index, arg) in args.iter().enumerate() {
        let Some(param) = signature.params.get(index + offset) else {
            break;
        };
        let name = param.name.trim();
        if name.is_empty() || name == "_" || name.starts_with("arg") {
            continue;
        }
        let obvious = matches!(
            &arg.kind,
            ExprKind::Lookup { path } if path.len() == 1 && path[0].text == name
        );
        if obvious {
            continue;
        }
        sink.add(
            arg.range.start,
            format!("{name}:"),
            InlayHintKind::PARAMETER,
            format!("`{name}` parameter of `{}`", signature.name),
            true,
        );
    }
}

fn pipe_before_expr(ctx: &FeatureContext<'_>, expr: &Expr) -> bool {
    let sig = significant(ctx.analysis);
    let Some(index) = sig
        .iter()
        .position(|token| token.start() >= expr.range.start)
    else {
        return false;
    };
    if index == 0 {
        return false;
    }
    is_pipe(sig[index - 1])
}

fn pattern_name(pattern: &Pattern) -> Option<&crate::syntax::ast::Name> {
    match &pattern.kind {
        PatternKind::Name { name, .. } => Some(name),
        _ => None,
    }
}

fn parse_function_type_text(text: &str) -> Option<(Vec<String>, String)> {
    let (left, right, _) = split_signature_arrow(text)?;
    let args = split_top_level(&left, ',')
        .into_iter()
        .filter(|piece| !piece.is_empty())
        .collect::<Vec<_>>();
    Some((args, right.unwrap_or_default()))
}

// ---------------------------------------------------------------------------
// Shared symbol lookup
// ---------------------------------------------------------------------------

fn find_symbol<'a>(
    analysis: &'a AnalyzedFile,
    name: &str,
    container: Option<&str>,
) -> Option<&'a SymbolInfo> {
    fn visit<'a>(
        symbols: &'a [SymbolInfo],
        name: &str,
        container: Option<&str>,
    ) -> Option<&'a SymbolInfo> {
        for symbol in symbols {
            if symbol.name == name
                && match container {
                    Some(wanted) => symbol.container.as_deref() == Some(wanted),
                    None => true,
                }
            {
                return Some(symbol);
            }
            if let Some(found) = visit(&symbol.children, name, container) {
                return Some(found);
            }
        }
        None
    }
    visit(&analysis.symbols, name, container)
}

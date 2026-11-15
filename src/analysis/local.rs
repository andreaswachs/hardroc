//! Local (single-file) analysis: scopes, bindings, symbols, references.
//!
//! Cross-file resolution is layered on top by [`super::host`].  This module is
//! intentionally pure: given a parsed file and read-only views of the workspace
//! and builtin indexes it produces an [`AnalyzedFile`] without doing I/O.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use lsp_types::Url;

use crate::syntax::ast::{
    Definition, Expr, ExprKind, Import, ImportPath, Item, ItemKind, Module, Name, ParsedFile,
    Pattern, PatternKind, QualName, StmtKind, StrPart, StrPartKind, TypeDecl, TypeExpr, TypeKind,
};
use crate::text::{SourceFile, TextRange};

use super::diag;
use super::index::{Export, ModuleKind, ModuleSummary};
use super::{
    AnalysisConfig, AnalysisDiagnostic, AnalyzedFile, BuiltinIndex, BuiltinKind, DefinitionKey,
    ParamInfo, Reference, ResolvedImport, ScopeInfo, Signature, SymbolInfo, SymbolKind,
    WorkspaceIndex,
};

/// Analyze one file.  `module_override` is used by the server for unsaved
/// buffers; otherwise the module name comes from the workspace index or the
/// filename.
pub fn analyze(
    uri: Url,
    source: Arc<SourceFile>,
    parsed: Arc<ParsedFile>,
    workspace: &WorkspaceIndex,
    builtins: &BuiltinIndex,
    _config: &AnalysisConfig,
) -> AnalyzedFile {
    let path = uri.to_file_path().ok();
    let workspace_module = workspace.module_for_uri(&uri);
    let module_name = workspace_module
        .map(|module| module.name.clone())
        .or_else(|| {
            path.as_deref()
                .and_then(|path| path.file_stem())
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "main".to_string());
    let module_kind = parsed
        .module
        .header
        .as_ref()
        .map(|header| match header.kind {
            crate::syntax::ast::HeaderKind::App => ModuleKind::App,
            crate::syntax::ast::HeaderKind::Module => ModuleKind::Module,
            crate::syntax::ast::HeaderKind::Package => ModuleKind::Package,
            crate::syntax::ast::HeaderKind::Platform => ModuleKind::Platform,
        })
        .or_else(|| workspace_module.map(|module| module.kind))
        .unwrap_or(ModuleKind::Module);

    let mut diagnostics = Vec::new();
    let imports = resolve_imports(
        &uri,
        path.as_deref(),
        &parsed.module,
        workspace,
        &mut diagnostics,
    );

    let import_symbols = import_symbols(&uri, &parsed.module.imports, &imports);
    let mut symbols = build_symbols(&uri, source.text(), &parsed.module, &mut diagnostics);
    symbols.extend(import_symbols);

    let (top_level, mut type_members) = collect_name_maps(&symbols);
    collect_record_fields(&parsed.module.items, &uri, &mut type_members);
    collect_tag_members(&parsed.module.items, &uri, &mut type_members);
    let mut record_fields = HashMap::new();
    let mut record_field_names = HashMap::new();
    let mut ambiguous_record_fields = HashSet::new();
    collect_record_type_fields(
        &parsed.module.items,
        &uri,
        &mut type_members,
        &mut record_fields,
        &mut record_field_names,
        &mut ambiguous_record_fields,
    );
    record_field_names.retain(|name, _| !ambiguous_record_fields.contains(name));
    let local_tags = collect_local_tags(&parsed.module.items, &uri);
    let resolver = Resolver::new(
        uri.clone(),
        builtins,
        top_level,
        type_members,
        local_tags,
        record_fields,
        record_field_names,
        imports,
    );
    let initial_references = import_references(&uri, &parsed.module.imports, &resolver);

    let parsed_clean = parsed.diagnostics.is_empty();
    let mut walker = Walker::new(
        uri.clone(),
        source.text(),
        resolver,
        &symbols,
        source.len(),
        initial_references,
        parsed_clean,
    );
    walker.walk_items(&parsed.module.items);
    let Walker {
        scopes,
        mut references,
        diagnostics: mut walker_diagnostics,
        resolver,
        ..
    } = walker;
    diagnostics.append(&mut walker_diagnostics);

    // Unused imports are only reported once every occurrence has been walked.
    for (index, import) in resolver.imports.iter().enumerate() {
        if !resolver.import_used[index] && !resolver.exposing_used[index] && import.error.is_none()
        {
            let mut diagnostic = AnalysisDiagnostic::warning(
                import.import.range,
                "unused-import",
                format!("import `{}` is never used", import.qualifier),
            );
            diagnostic.fixes.push(super::DiagnosticFix {
                title: "Remove unused import".to_string(),
                edits: vec![super::SuggestedEdit {
                    uri: uri.clone(),
                    range: import.import.range,
                    new_text: String::new(),
                }],
            });
            diagnostics.push(diagnostic);
        }
    }

    let mut signatures = collect_signatures(&uri, source.text(), &parsed.module);
    add_builtin_signatures(&mut signatures, &references, builtins);

    references.sort_by_key(|reference| (reference.range.start, reference.range.end));

    AnalyzedFile {
        uri,
        source: source.clone(),
        parsed,
        module_name,
        module_kind,
        symbols,
        references,
        scopes,
        imports: resolver.imports,
        diagnostics,
        signatures,
    }
}

pub fn empty_import(import: Import) -> ResolvedImport {
    ResolvedImport {
        qualifier: import.path.raw().to_string(),
        import,
        target: None,
        file_uri: None,
        exposing: Vec::new(),
        nested: Vec::new(),
        error: None,
    }
}

pub fn synthetic_symbol(
    name: impl Into<String>,
    range: TextRange,
    kind: super::SymbolKind,
) -> SymbolInfo {
    SymbolInfo {
        name: name.into(),
        kind,
        name_range: range,
        full_range: range,
        detail: None,
        doc: None,
        container: None,
        key: super::DefinitionKey::Local {
            uri: Url::parse("file:///unknown").unwrap(),
            def_start: range.start,
        },
        scope: 0,
        children: Vec::new(),
    }
}

pub fn empty_scope(range: TextRange) -> ScopeInfo {
    ScopeInfo {
        id: 0,
        range,
        parent: None,
        bindings: Vec::new(),
    }
}

#[derive(Debug, Clone)]
struct ResolvedName {
    key: DefinitionKey,
    kind: SymbolKind,
}

impl ResolvedName {
    fn from_export(export: &Export, summary: &ModuleSummary) -> Self {
        Self {
            key: export.key.clone().unwrap_or_else(|| DefinitionKey::Module {
                uri: summary.uri.clone(),
            }),
            kind: export.kind,
        }
    }
}

/* ------------------------------------------------------------------------- */
/* Imports                                                                   */
/* ------------------------------------------------------------------------- */

fn resolve_imports(
    _uri: &Url,
    path: Option<&Path>,
    module: &Module,
    workspace: &WorkspaceIndex,
    diagnostics: &mut Vec<AnalysisDiagnostic>,
) -> Vec<ResolvedImport> {
    let mut output = Vec::new();
    let directory = path.and_then(Path::parent);
    for import in &module.imports {
        let (qualifier, _qualifier_range) = import_qualifier(import);
        let mut resolved = ResolvedImport {
            qualifier,
            import: import.clone(),
            target: None,
            file_uri: None,
            exposing: import
                .exposing
                .iter()
                .map(|name| name.text.clone())
                .collect(),
            nested: Vec::new(),
            error: None,
        };
        match &import.path {
            ImportPath::File { path: raw, .. } => {
                let mut missing = true;
                if let Some(directory) = directory {
                    let target = super::packages::normalize_path(&directory.join(raw));
                    if target.is_file() {
                        resolved.file_uri = super::packages::uri_for_path(&target);
                        missing = false;
                    }
                }
                if missing {
                    resolved.error = Some(format!("file `{raw}` could not be found"));
                }
            }
            ImportPath::Roc { raw, segments, .. } => {
                let segment_names = segments
                    .iter()
                    .map(|name| name.text.as_str())
                    .collect::<Vec<_>>();
                let target = find_import_target(workspace, directory, raw, segment_names);
                match target {
                    Some((summary, nested)) => {
                        resolved.file_uri = Some(summary.uri.clone());
                        for name in &import.exposing {
                            if summary.export(&name.text).is_none() {
                                diagnostics.push(AnalysisDiagnostic::error(
                                    name.range,
                                    "unknown-exposing-name",
                                    format!("`{}` is not exported by module `{}`", name.text, raw),
                                ));
                            }
                        }
                        resolved.nested = nested;
                        resolved.target = Some(summary.clone());
                    }
                    None => {
                        let compiler_owned = raw
                            .split(['.', '/'])
                            .next()
                            .and_then(|first| workspace.package(first))
                            .is_some_and(|package| {
                                matches!(
                                    package.source,
                                    crate::syntax::ast::DependencySource::CompilerOwned(_)
                                ) && package.root.is_none()
                            });
                        if !compiler_owned {
                            resolved.error = Some(format!("module `{raw}` could not be found"));
                        }
                    }
                }
            }
        }
        if let Some(error) = &resolved.error {
            let mut diagnostic = diag::unknown_module(import.path.range(), import.path.raw());
            diagnostic.message = format!("{}: {}", diagnostic.message, error);
            diagnostic.code = "unresolved-import".to_string();
            diagnostics.push(diagnostic);
        }
        output.push(resolved);
    }
    output
}

/// Resolve an import, including nested type/value imports whose module is a
/// prefix of the written path (`json.Parser.ParseErr`).
fn find_import_target(
    workspace: &WorkspaceIndex,
    directory: Option<&Path>,
    raw: &str,
    segments: Vec<&str>,
) -> Option<(ModuleSummary, Vec<String>)> {
    if let Some(summary) = find_import_module(workspace, directory, raw, segments.clone()) {
        return Some((summary, Vec::new()));
    }
    let parts = raw
        .split(['.', '/'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    for end in (1..parts.len()).rev() {
        let prefix = parts[..end].join(".");
        let Some(summary) =
            find_import_module(workspace, directory, &prefix, parts[..end].to_vec())
        else {
            continue;
        };
        let nested = parts[end..]
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        // Ensure the final segment really is exported by the module.
        if nested
            .last()
            .is_some_and(|name| summary.export(name).is_some())
        {
            return Some((summary, nested));
        }
    }
    None
}

fn find_import_module(
    workspace: &WorkspaceIndex,
    directory: Option<&Path>,
    raw: &str,
    segments: Vec<&str>,
) -> Option<ModuleSummary> {
    let root_relative = raw.starts_with('/');
    let raw = raw
        .trim_end_matches(".roc")
        .trim_start_matches("./")
        .trim_start_matches('/');
    let mut candidates: Vec<String> = vec![raw.to_string()];
    if raw.contains('.') {
        candidates.push(raw.replace('.', "/"));
    }
    if raw.contains('/') {
        candidates.push(raw.replace('/', "."));
    }
    let package_alias = raw
        .split(['.', '/'])
        .next()
        .filter(|first| {
            first
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_lowercase())
                && workspace.package(first).is_some()
        })
        .map(str::to_string);
    // A module file next to the importing file is the most precise match.
    // `/Module` is relative to the workspace root instead. Package-qualified
    // imports resolve through the package's public module table.
    if package_alias.is_none() {
        if let Some(directory) = if root_relative {
            Some(workspace.root.as_path())
        } else {
            directory
        } {
            for candidate in &candidates {
                let file = if candidate.ends_with(".roc") {
                    directory.join(candidate)
                } else {
                    directory.join(format!("{candidate}.roc"))
                };
                let path = super::packages::normalize_path(&file);
                if let Some(summary) = workspace.module_for_path(&path) {
                    if !workspace.is_package_root(&summary.path) {
                        return Some(summary.clone());
                    }
                }
            }
        }
    }

    let mut parts: Vec<String> = segments.iter().map(|part| part.to_string()).collect();
    if parts.is_empty() {
        parts = raw.split(['.', '/']).map(str::to_string).collect();
    }
    parts.retain(|part| !part.is_empty());
    let package = package_alias;
    let mut module_candidates: Vec<String> = Vec::new();
    if package.is_some() && parts.len() > 1 {
        module_candidates.push(parts[1..].join("."));
        module_candidates.push(parts[1..].join("/"));
    }
    module_candidates.extend(candidates.iter().cloned());
    if package.is_none() {
        if let Some(last) = parts.last() {
            module_candidates.push(last.clone());
        }
    }
    module_candidates.dedup();
    for candidate in &module_candidates {
        let summary = match package.as_deref() {
            Some(package) => workspace.find_public_module(package, candidate),
            None => workspace.find_module(None, candidate),
        };
        if let Some(summary) = summary {
            return Some(summary.clone());
        }
    }
    None
}

fn import_qualifier(import: &Import) -> (String, TextRange) {
    if let Some(alias) = &import.alias {
        return (alias.text.clone(), alias.range);
    }
    match &import.path {
        ImportPath::Roc {
            raw,
            segments,
            range,
        } => {
            let last = segments.last();
            let text = last
                .map(|name| name.text.clone())
                .unwrap_or_else(|| raw.rsplit(['.', '/']).next().unwrap_or(raw).to_string());
            let text = text.rsplit('/').next().unwrap_or(&text).to_string();
            let text_range = last.map(|name| name.range).unwrap_or(*range);
            (text, text_range)
        }
        ImportPath::File { path, range } => {
            let stem = Path::new(path.as_str())
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or(path)
                .to_string();
            (stem, *range)
        }
    }
}

fn import_key(uri: &Url, resolved: &ResolvedImport) -> DefinitionKey {
    if let Some(target) = &resolved.target {
        if let Some(name) = resolved.nested.last() {
            if let Some(key) = target.export(name).and_then(|export| export.key.clone()) {
                return key;
            }
        }
        return DefinitionKey::Module {
            uri: target.uri.clone(),
        };
    }
    if let Some(file) = &resolved.file_uri {
        return DefinitionKey::Module { uri: file.clone() };
    }
    let (_, range) = import_qualifier(&resolved.import);
    DefinitionKey::Local {
        uri: uri.clone(),
        def_start: range.start,
    }
}

fn import_symbols(uri: &Url, imports: &[Import], resolved: &[ResolvedImport]) -> Vec<SymbolInfo> {
    let mut output = Vec::new();
    for (import, resolved) in imports.iter().zip(resolved.iter()) {
        let (qualifier, range) = import_qualifier(import);
        output.push(SymbolInfo {
            name: qualifier,
            kind: SymbolKind::Import,
            name_range: range,
            full_range: import.range,
            detail: Some(import.path.raw().to_string()),
            doc: None,
            container: None,
            key: import_key(uri, resolved),
            scope: 0,
            children: Vec::new(),
        });
    }
    output
}

fn import_references(uri: &Url, imports: &[Import], resolver: &Resolver<'_>) -> Vec<Reference> {
    let mut output = Vec::new();
    for (import, resolved) in imports.iter().zip(resolver.imports.iter()) {
        let (qualifier, range) = import_qualifier(import);
        output.push(Reference {
            range,
            text: qualifier,
            key: Some(import_key(uri, resolved)),
            kind: SymbolKind::Import,
        });
        if let Some(target) = &resolved.target {
            for name in &import.exposing {
                let export = target.export(&name.text);
                output.push(Reference {
                    range: name.range,
                    text: name.text.clone(),
                    key: export.and_then(|export| export.key.clone()),
                    kind: export
                        .map(|export| export.kind)
                        .unwrap_or(SymbolKind::Import),
                });
            }
        }
    }
    output
}

/* ------------------------------------------------------------------------- */
/* Symbols                                                                   */
/* ------------------------------------------------------------------------- */

fn build_symbols(
    uri: &Url,
    source: &str,
    module: &Module,
    diagnostics: &mut Vec<AnalysisDiagnostic>,
) -> Vec<SymbolInfo> {
    let mut builder = SymbolBuilder {
        uri,
        source,
        diagnostics,
        seen: HashMap::new(),
    };
    let mut symbols = Vec::new();
    builder.items(&module.items, None, &mut symbols);
    symbols
}

struct SymbolBuilder<'a> {
    uri: &'a Url,
    source: &'a str,
    diagnostics: &'a mut Vec<AnalysisDiagnostic>,
    seen: HashMap<String, TextRange>,
}

impl<'a> SymbolBuilder<'a> {
    fn items(&mut self, items: &[Item], container: Option<&str>, output: &mut Vec<SymbolInfo>) {
        for item in items {
            match &item.kind {
                ItemKind::Definition(definition) => self.definition(definition, container, output),
                ItemKind::Annotation(annotation) => {
                    let kind = if is_function_type(&annotation.ty) {
                        SymbolKind::Function
                    } else {
                        SymbolKind::Constant
                    };
                    let detail = Some(slice(self.source, annotation.ty.range));
                    let symbol = self.symbol(
                        annotation.name.text.clone(),
                        kind,
                        annotation.name.range,
                        annotation.range,
                        detail,
                        annotation
                            .doc
                            .as_ref()
                            .map(|doc| doc.text.trim().to_string()),
                        container,
                        Vec::new(),
                    );
                    output.push(symbol);
                }
                ItemKind::TypeDecl(decl) => self.type_decl(decl, container, output),
                ItemKind::WhereAlias(alias) => {
                    let detail = Some(format!("where {} requirement(s)", alias.requirements.len()));
                    let symbol = self.symbol(
                        alias.name.text.clone(),
                        SymbolKind::Type,
                        alias.name.range,
                        alias.range,
                        detail,
                        alias.doc.as_ref().map(|doc| doc.text.trim().to_string()),
                        container,
                        Vec::new(),
                    );
                    output.push(symbol);
                }
                ItemKind::Expect(_) | ItemKind::Error => {}
            }
        }
    }

    fn definition(
        &mut self,
        definition: &Definition,
        container: Option<&str>,
        output: &mut Vec<SymbolInfo>,
    ) {
        let kind = definition_kind(definition, container.is_some());
        let detail = definition
            .annotation
            .as_ref()
            .map(|annotation| slice(self.source, annotation.range));
        let doc = definition
            .doc
            .as_ref()
            .map(|doc| doc.text.trim().to_string());
        if let Some(name) = definition.simple_name() {
            if definition.is_var && container.is_none() {
                self.diagnostics.push(AnalysisDiagnostic::error(
                    name.range,
                    "var-at-top-level",
                    "`var` bindings are only allowed inside functions",
                ));
            }
            output.push(self.symbol(
                name.text.clone(),
                kind,
                name.range,
                definition.range,
                detail,
                doc,
                container,
                Vec::new(),
            ));
            return;
        }
        let mut bindings = Vec::new();
        definition.pattern.bindings(&mut bindings);
        for (name, range, is_var) in bindings {
            if name == "_" {
                continue;
            }
            let kind = if container.is_some() {
                SymbolKind::Method
            } else if is_var {
                SymbolKind::Variable
            } else {
                SymbolKind::Constant
            };
            let symbol = self.symbol(
                name,
                kind,
                range,
                definition.range,
                detail.clone(),
                doc.clone(),
                container,
                Vec::new(),
            );
            output.push(symbol);
        }
    }

    fn type_decl(
        &mut self,
        decl: &TypeDecl,
        container: Option<&str>,
        output: &mut Vec<SymbolInfo>,
    ) {
        let child_container = match container {
            Some(container) => format!("{container}.{}", decl.name.text),
            None => decl.name.text.clone(),
        };
        let mut children = Vec::new();
        for param in &decl.params {
            children.push(SymbolInfo {
                name: param.text.clone(),
                kind: SymbolKind::TypeParameter,
                name_range: param.range,
                full_range: param.range,
                detail: None,
                doc: None,
                container: Some(child_container.clone()),
                key: DefinitionKey::Local {
                    uri: self.uri.clone(),
                    def_start: param.range.start,
                },
                scope: 0,
                children: Vec::new(),
            });
        }
        self.items(&decl.members, Some(&child_container), &mut children);
        let symbol = self.symbol(
            decl.name.text.clone(),
            SymbolKind::Type,
            decl.name.range,
            decl.range,
            Some(slice(self.source, decl.backing.range)),
            decl.doc.as_ref().map(|doc| doc.text.trim().to_string()),
            container,
            children,
        );
        output.push(symbol);
    }

    #[allow(clippy::too_many_arguments)]
    fn symbol(
        &mut self,
        name: String,
        kind: SymbolKind,
        name_range: TextRange,
        full_range: TextRange,
        detail: Option<String>,
        doc: Option<String>,
        container: Option<&str>,
        children: Vec<SymbolInfo>,
    ) -> SymbolInfo {
        if container.is_none() {
            if let Some(previous) = self.seen.get(&name) {
                let mut diagnostic = AnalysisDiagnostic::warning(
                    name_range,
                    "duplicate-definition",
                    format!("`{name}` is defined more than once"),
                );
                diagnostic.related.push((
                    self.uri.clone(),
                    *previous,
                    "first definition is here".to_string(),
                ));
                self.diagnostics.push(diagnostic);
            } else {
                self.seen.insert(name.clone(), name_range);
            }
        }
        SymbolInfo {
            name: name.clone(),
            kind,
            name_range,
            full_range,
            detail,
            doc,
            container: container.map(str::to_string),
            key: DefinitionKey::TopLevel {
                uri: self.uri.clone(),
                name,
                container: container.map(str::to_string),
            },
            scope: 0,
            children,
        }
    }
}

fn collect_name_maps(
    symbols: &[SymbolInfo],
) -> (HashMap<String, ResolvedName>, HashMap<String, ResolvedName>) {
    let mut top = HashMap::new();
    let mut members = HashMap::new();
    collect_top_level(symbols, &mut top);
    collect_members(symbols, &mut members);
    (top, members)
}

fn collect_top_level(symbols: &[SymbolInfo], top: &mut HashMap<String, ResolvedName>) {
    for symbol in symbols {
        if symbol.kind == SymbolKind::Import {
            continue;
        }
        if symbol.container.is_none() {
            top.entry(symbol.name.clone()).or_insert(ResolvedName {
                key: symbol.key.clone(),
                kind: symbol.kind,
            });
        }
        collect_top_level(&symbol.children, top);
    }
}

fn collect_members(symbols: &[SymbolInfo], members: &mut HashMap<String, ResolvedName>) {
    for symbol in symbols {
        if let Some(container) = &symbol.container {
            members
                .entry(format!("{}.{}", container, symbol.name))
                .or_insert(ResolvedName {
                    key: symbol.key.clone(),
                    kind: symbol.kind,
                });
        }
        collect_members(&symbol.children, members);
    }
}

fn collect_record_fields(items: &[Item], uri: &Url, members: &mut HashMap<String, ResolvedName>) {
    for item in items {
        match &item.kind {
            ItemKind::Definition(definition) => {
                let Some(name) = definition.simple_name() else {
                    continue;
                };
                let ExprKind::Record { fields, .. } = &definition.value.kind else {
                    continue;
                };
                for field in fields {
                    members
                        .entry(format!("{}.{}", name.text, field.name.text))
                        .or_insert_with(|| ResolvedName {
                            key: DefinitionKey::Local {
                                uri: uri.clone(),
                                def_start: field.name.range.start,
                            },
                            kind: SymbolKind::Field,
                        });
                }
            }
            ItemKind::TypeDecl(decl) => {
                collect_record_fields(&decl.members, uri, members);
            }
            _ => {}
        }
    }
}

fn collect_tag_members(items: &[Item], uri: &Url, members: &mut HashMap<String, ResolvedName>) {
    for item in items {
        let ItemKind::TypeDecl(decl) = &item.kind else {
            continue;
        };
        collect_tag_members(&decl.members, uri, members);
        if let TypeKind::TagUnion { tags, .. } = &decl.backing.kind {
            for tag in tags {
                members
                    .entry(format!("{}.{}", decl.name.text, tag.name.text))
                    .or_insert_with(|| ResolvedName {
                        key: DefinitionKey::Local {
                            uri: uri.clone(),
                            def_start: tag.name.range.start,
                        },
                        kind: SymbolKind::Tag,
                    });
            }
        }
    }
}

fn collect_record_type_fields(
    items: &[Item],
    uri: &Url,
    members: &mut HashMap<String, ResolvedName>,
    declarations: &mut HashMap<u32, ResolvedName>,
    names: &mut HashMap<String, ResolvedName>,
    ambiguous: &mut HashSet<String>,
) {
    for item in items {
        let ItemKind::TypeDecl(decl) = &item.kind else {
            continue;
        };
        if let TypeKind::Record { fields, .. } = &decl.backing.kind {
            for field in fields {
                let resolved = ResolvedName {
                    key: DefinitionKey::Local {
                        uri: uri.clone(),
                        def_start: field.name.range.start,
                    },
                    kind: SymbolKind::Field,
                };
                members
                    .entry(format!("{}.{}", decl.name.text, field.name.text))
                    .or_insert_with(|| resolved.clone());
                declarations.insert(field.name.range.start, resolved.clone());
                if names.insert(field.name.text.clone(), resolved).is_some() {
                    ambiguous.insert(field.name.text.clone());
                }
            }
        }
        collect_record_type_fields(&decl.members, uri, members, declarations, names, ambiguous);
    }
}

fn collect_local_tags(items: &[Item], uri: &Url) -> HashMap<String, ResolvedName> {
    let mut tags: HashMap<String, ResolvedName> = HashMap::new();
    let mut ambiguous = HashSet::new();
    for item in items {
        if let ItemKind::TypeDecl(decl) = &item.kind {
            collect_tags_in_type(&decl.backing, uri, &mut tags, &mut ambiguous);
            collect_local_tags_inner(&decl.members, uri, &mut tags, &mut ambiguous);
        }
    }
    tags.retain(|name, _| !ambiguous.contains(name));
    tags
}

fn collect_local_tags_inner(
    items: &[Item],
    uri: &Url,
    tags: &mut HashMap<String, ResolvedName>,
    ambiguous: &mut HashSet<String>,
) {
    for item in items {
        if let ItemKind::TypeDecl(decl) = &item.kind {
            collect_tags_in_type(&decl.backing, uri, tags, ambiguous);
            collect_local_tags_inner(&decl.members, uri, tags, ambiguous);
        }
    }
}

fn collect_tags_in_type(
    ty: &TypeExpr,
    uri: &Url,
    tags: &mut HashMap<String, ResolvedName>,
    ambiguous: &mut HashSet<String>,
) {
    match &ty.kind {
        TypeKind::TagUnion {
            tags: union_tags, ..
        } => {
            for tag in union_tags {
                let resolved = ResolvedName {
                    key: DefinitionKey::Local {
                        uri: uri.clone(),
                        def_start: tag.name.range.start,
                    },
                    kind: SymbolKind::Tag,
                };
                if tags.insert(tag.name.text.clone(), resolved).is_some() {
                    ambiguous.insert(tag.name.text.clone());
                }
                for payload in &tag.payloads {
                    collect_tags_in_type(payload, uri, tags, ambiguous);
                }
            }
        }
        TypeKind::Function { args, ret, .. } => {
            for arg in args {
                collect_tags_in_type(arg, uri, tags, ambiguous);
            }
            collect_tags_in_type(ret, uri, tags, ambiguous);
        }
        TypeKind::Tuple(types) => {
            for ty in types {
                collect_tags_in_type(ty, uri, tags, ambiguous);
            }
        }
        TypeKind::Record { fields, .. } => {
            for field in fields {
                collect_tags_in_type(&field.ty, uri, tags, ambiguous);
            }
        }
        TypeKind::Apply { args, .. } => {
            for arg in args {
                collect_tags_in_type(arg, uri, tags, ambiguous);
            }
        }
        TypeKind::Where { base, requirements } => {
            collect_tags_in_type(base, uri, tags, ambiguous);
            for requirement in requirements {
                collect_tags_in_type(&requirement.ty, uri, tags, ambiguous);
            }
        }
        TypeKind::Named(_) | TypeKind::Var(_) | TypeKind::Wildcard | TypeKind::Error => {}
    }
}

fn definition_kind(definition: &Definition, in_type: bool) -> SymbolKind {
    if in_type {
        SymbolKind::Method
    } else if definition.is_var {
        SymbolKind::Variable
    } else if definition.annotation.as_ref().is_some_and(is_function_type)
        || matches!(definition.value.kind, ExprKind::Lambda { .. })
    {
        SymbolKind::Function
    } else {
        SymbolKind::Constant
    }
}

fn is_function_type(ty: &TypeExpr) -> bool {
    matches!(ty.kind, TypeKind::Function { .. })
}

/* ------------------------------------------------------------------------- */
/* Resolver                                                                  */
/* ------------------------------------------------------------------------- */

struct Resolver<'a> {
    uri: Url,
    builtins: &'a BuiltinIndex,
    top_level: HashMap<String, ResolvedName>,
    type_members: HashMap<String, ResolvedName>,
    tags: HashMap<String, ResolvedName>,
    record_fields: HashMap<u32, ResolvedName>,
    record_field_names: HashMap<String, ResolvedName>,
    imports: Vec<ResolvedImport>,
    import_index: HashMap<String, usize>,
    import_used: Vec<bool>,
    exposing: HashMap<String, (ResolvedName, usize)>,
    exposing_used: Vec<bool>,
}

impl<'a> Resolver<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        uri: Url,
        builtins: &'a BuiltinIndex,
        top_level: HashMap<String, ResolvedName>,
        type_members: HashMap<String, ResolvedName>,
        tags: HashMap<String, ResolvedName>,
        record_fields: HashMap<u32, ResolvedName>,
        record_field_names: HashMap<String, ResolvedName>,
        imports: Vec<ResolvedImport>,
    ) -> Self {
        let mut import_index = HashMap::new();
        for (index, import) in imports.iter().enumerate() {
            import_index
                .entry(import.qualifier.clone())
                .or_insert(index);
        }
        let mut exposing = HashMap::new();
        for (index, import) in imports.iter().enumerate() {
            let Some(target) = &import.target else {
                continue;
            };
            for name in &import.exposing {
                if let Some(export) = target.export(name) {
                    exposing
                        .entry(name.clone())
                        .or_insert((ResolvedName::from_export(export, target), index));
                }
            }
        }
        let import_used = vec![false; imports.len()];
        let exposing_used = vec![false; imports.len()];
        Self {
            uri,
            builtins,
            top_level,
            type_members,
            tags,
            record_fields,
            record_field_names,
            imports,
            import_index,
            import_used,
            exposing,
            exposing_used,
        }
    }

    fn resolve_unqualified(&mut self, name: &str) -> Option<ResolvedName> {
        if let Some(found) = self.top_level.get(name) {
            return Some(found.clone());
        }
        if let Some((found, index)) = self.exposing.get(name).cloned() {
            if let Some(slot) = self.exposing_used.get_mut(index) {
                *slot = true;
            }
            return Some(found);
        }
        if let Some(found) = self.import_binding(name) {
            return Some(found);
        }
        if let Some(item) = self.builtins.lookup(name) {
            return Some(ResolvedName {
                key: item.key(),
                kind: builtin_symbol_kind(item.kind),
            });
        }
        if self.builtins.module(name).is_some() {
            return Some(ResolvedName {
                key: DefinitionKey::Builtin {
                    full_name: name.to_string(),
                },
                kind: SymbolKind::Module,
            });
        }
        if is_builtin_tag(name) {
            return Some(ResolvedName {
                key: DefinitionKey::Builtin {
                    full_name: name.to_string(),
                },
                kind: SymbolKind::Tag,
            });
        }
        None
    }

    fn import_binding(&mut self, name: &str) -> Option<ResolvedName> {
        let index = *self.import_index.get(name)?;
        if let Some(slot) = self.import_used.get_mut(index) {
            *slot = true;
        }
        let import = self.imports.get(index)?;
        if let Some(target) = &import.target {
            if let Some(name) = import.nested.last() {
                if let Some(export) = target.export(name) {
                    return Some(ResolvedName::from_export(export, target));
                }
            }
            return Some(ResolvedName {
                key: DefinitionKey::Module {
                    uri: target.uri.clone(),
                },
                kind: SymbolKind::Module,
            });
        }
        if let Some(uri) = &import.file_uri {
            return Some(ResolvedName {
                key: DefinitionKey::Module { uri: uri.clone() },
                kind: SymbolKind::Module,
            });
        }
        let (_, range) = import_qualifier(&import.import);
        Some(ResolvedName {
            key: DefinitionKey::Local {
                uri: self.uri.clone(),
                def_start: range.start,
            },
            kind: SymbolKind::Import,
        })
    }

    fn import_target(&mut self, name: &str) -> Option<ModuleSummary> {
        let index = *self.import_index.get(name)?;
        if let Some(slot) = self.import_used.get_mut(index) {
            *slot = true;
        }
        self.imports.get(index)?.target.clone()
    }

    fn resolve_path(&mut self, path: &[Name]) -> Option<ResolvedName> {
        if path.is_empty() {
            return None;
        }
        if path.len() == 1 {
            return self.resolve_unqualified(&path[0].text);
        }
        let joined = join_names(path);
        if let Some(found) = self.type_members.get(&joined) {
            return Some(found.clone());
        }
        let first = &path[0];
        if let Some(target) = self.import_target(&first.text) {
            return resolve_in_summary(&target, &path[1..]);
        }
        if self.import_index.contains_key(&first.text) {
            return None;
        }
        if let Some(found) = self.top_level.get(&first.text) {
            if found.kind == SymbolKind::Type {
                return self.type_members.get(&joined).cloned();
            }
        }
        if let Some(item) = self.builtins.lookup(&joined) {
            return Some(ResolvedName {
                key: item.key(),
                kind: builtin_symbol_kind(item.kind),
            });
        }
        if let Some(base) = self.builtins.lookup(&first.text) {
            // Builtin types live under their defining module (`Num.I64`), but
            // Roc source spells them with the type name alone (`I64.to_f64`).
            let member = path[1..]
                .iter()
                .map(|name| name.text.as_str())
                .collect::<Vec<_>>()
                .join(".");
            let candidate = format!("{}.{}", base.name, member);
            if let Some(item) = self.builtins.lookup(&candidate) {
                return Some(ResolvedName {
                    key: item.key(),
                    kind: builtin_symbol_kind(item.kind),
                });
            }
        }
        if self.builtins.module(&first.text).is_some() {
            return None;
        }
        None
    }

    fn resolve_tag(&self, name: &str) -> Option<ResolvedName> {
        if let Some(tag) = self.tags.get(name) {
            return Some(tag.clone());
        }
        if is_builtin_tag(name) {
            return Some(ResolvedName {
                key: DefinitionKey::Builtin {
                    full_name: name.to_string(),
                },
                kind: SymbolKind::Tag,
            });
        }
        self.builtins.lookup(name).map(|item| ResolvedName {
            key: item.key(),
            kind: builtin_symbol_kind(item.kind),
        })
    }
}

fn resolve_in_summary(summary: &ModuleSummary, rest: &[Name]) -> Option<ResolvedName> {
    let exports = summary.all_exports();
    let first = rest.first()?;
    let mut found = exports
        .iter()
        .copied()
        .find(|export| export.name == first.text)?;
    for name in &rest[1..] {
        if let Some(child) = found.children.iter().find(|child| child.name == name.text) {
            found = child;
        } else {
            let sibling = exports
                .iter()
                .copied()
                .find(|export| export.name == name.text && export.container == found.container)?;
            // `Color.base.to_hex`: `base` is a value member of the `Color`
            // nominal type, and `to_hex` is a sibling method on that type.
            found = sibling;
        }
    }
    Some(ResolvedName::from_export(found, summary))
}

fn builtin_symbol_kind(kind: BuiltinKind) -> SymbolKind {
    match kind {
        BuiltinKind::Type => SymbolKind::Type,
        BuiltinKind::Value => SymbolKind::Function,
        BuiltinKind::Tag => SymbolKind::Tag,
    }
}

fn is_builtin_tag(name: &str) -> bool {
    matches!(name, "True" | "False" | "Ok" | "Err" | "Some" | "None")
}

fn join_names(path: &[Name]) -> String {
    path.iter()
        .map(|name| name.text.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/* ------------------------------------------------------------------------- */
/* Walker                                                                    */
/* ------------------------------------------------------------------------- */

struct Walker<'a> {
    uri: Url,
    source: &'a str,
    resolver: Resolver<'a>,
    scopes: Vec<ScopeInfo>,
    scope_stack: Vec<u32>,
    references: Vec<Reference>,
    diagnostics: Vec<AnalysisDiagnostic>,
    reported: HashSet<(u32, u32)>,
    parsed_clean: bool,
}

impl<'a> Walker<'a> {
    fn new(
        uri: Url,
        source: &'a str,
        resolver: Resolver<'a>,
        module_symbols: &[SymbolInfo],
        file_len: u32,
        initial_references: Vec<Reference>,
        parsed_clean: bool,
    ) -> Self {
        let module_scope = ScopeInfo {
            id: 0,
            range: TextRange::new(0, file_len),
            parent: None,
            // Imports are not ordinary module bindings: resolving through the
            // resolver is what marks an import as used. Keeping import symbols
            // in scope would bypass that bookkeeping and produce false
            // `unused-import` warnings for every imported module.
            bindings: module_symbols
                .iter()
                .filter(|symbol| symbol.kind != SymbolKind::Import)
                .cloned()
                .collect(),
        };
        Self {
            uri,
            source,
            resolver,
            scopes: vec![module_scope],
            scope_stack: vec![0],
            references: initial_references,
            diagnostics: Vec::new(),
            reported: HashSet::new(),
            parsed_clean,
        }
    }

    fn walk_items(&mut self, items: &[Item]) {
        self.walk_items_in(items, None);
    }

    fn walk_items_in(&mut self, items: &[Item], container: Option<&str>) {
        for item in items {
            match &item.kind {
                ItemKind::Definition(definition) => self.walk_definition(definition, container),
                ItemKind::Annotation(annotation) => {
                    let kind = if is_function_type(&annotation.ty) {
                        SymbolKind::Function
                    } else {
                        SymbolKind::Constant
                    };
                    let key = DefinitionKey::TopLevel {
                        uri: self.uri.clone(),
                        name: annotation.name.text.clone(),
                        container: container.map(str::to_string),
                    };
                    self.references.push(Reference {
                        range: annotation.name.range,
                        text: annotation.name.text.clone(),
                        key: Some(key),
                        kind,
                    });
                    self.walk_type(&annotation.ty);
                }
                ItemKind::TypeDecl(decl) => self.walk_type_decl(decl, container),
                ItemKind::WhereAlias(alias) => {
                    let key = DefinitionKey::TopLevel {
                        uri: self.uri.clone(),
                        name: alias.name.text.clone(),
                        container: container.map(str::to_string),
                    };
                    self.references.push(Reference {
                        range: alias.name.range,
                        text: alias.name.text.clone(),
                        key: Some(key),
                        kind: SymbolKind::Type,
                    });
                    for requirement in &alias.requirements {
                        self.walk_type(&requirement.ty);
                    }
                }
                ItemKind::Expect(expect) => self.walk_expr(&expect.expr),
                ItemKind::Error => {}
            }
        }
    }

    fn walk_definition(&mut self, definition: &Definition, container: Option<&str>) {
        let kind = definition_kind(definition, container.is_some());
        if let Some(name) = definition.simple_name() {
            let key = DefinitionKey::TopLevel {
                uri: self.uri.clone(),
                name: name.text.clone(),
                container: container.map(str::to_string),
            };
            self.references.push(Reference {
                range: name.range,
                text: name.text.clone(),
                key: Some(key),
                kind,
            });
        } else {
            let mut bindings = Vec::new();
            definition.pattern.bindings(&mut bindings);
            for (name, range, _) in bindings {
                if name == "_" {
                    continue;
                }
                let key = DefinitionKey::TopLevel {
                    uri: self.uri.clone(),
                    name: name.clone(),
                    container: container.map(str::to_string),
                };
                self.references.push(Reference {
                    range,
                    text: name,
                    key: Some(key),
                    kind: if container.is_some() {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Constant
                    },
                });
            }
            self.walk_pattern_refs(&definition.pattern);
        }
        if let Some(annotation) = &definition.annotation {
            self.walk_type(annotation);
        }
        self.walk_expr(&definition.value);
    }

    fn walk_local_definition(&mut self, definition: &Definition) {
        if let Some(annotation) = &definition.annotation {
            self.walk_type(annotation);
        }
        self.walk_expr(&definition.value);
        let scope = self.scope_stack.last().copied().unwrap_or(0);
        if let Some(name) = definition.simple_name() {
            let key = DefinitionKey::Local {
                uri: self.uri.clone(),
                def_start: name.range.start,
            };
            let kind = if definition.is_var || name.text.starts_with('$') {
                SymbolKind::Variable
            } else {
                SymbolKind::Local
            };
            self.add_scope_binding(
                scope,
                SymbolInfo {
                    name: name.text.clone(),
                    kind,
                    name_range: name.range,
                    full_range: definition.range,
                    detail: definition
                        .annotation
                        .as_ref()
                        .map(|annotation| slice(self.source, annotation.range)),
                    doc: definition
                        .doc
                        .as_ref()
                        .map(|doc| doc.text.trim().to_string()),
                    container: None,
                    key: key.clone(),
                    scope,
                    children: Vec::new(),
                },
            );
            self.references.push(Reference {
                range: name.range,
                text: name.text.clone(),
                key: Some(key),
                kind,
            });
        } else {
            self.bind_pattern(&definition.pattern, SymbolKind::Local);
        }
    }

    fn walk_type_decl(&mut self, decl: &TypeDecl, outer: Option<&str>) {
        let child_container = match outer {
            Some(outer) => format!("{outer}.{}", decl.name.text),
            None => decl.name.text.clone(),
        };
        let key = DefinitionKey::TopLevel {
            uri: self.uri.clone(),
            name: decl.name.text.clone(),
            container: outer.map(str::to_string),
        };
        self.references.push(Reference {
            range: decl.name.range,
            text: decl.name.text.clone(),
            key: Some(key),
            kind: SymbolKind::Type,
        });
        let scope = self.push_scope(decl.range);
        for param in &decl.params {
            let key = DefinitionKey::Local {
                uri: self.uri.clone(),
                def_start: param.range.start,
            };
            self.add_scope_binding(
                scope,
                SymbolInfo {
                    name: param.text.clone(),
                    kind: SymbolKind::TypeParameter,
                    name_range: param.range,
                    full_range: param.range,
                    detail: None,
                    doc: None,
                    container: Some(child_container.clone()),
                    key: key.clone(),
                    scope,
                    children: Vec::new(),
                },
            );
            self.references.push(Reference {
                range: param.range,
                text: param.text.clone(),
                key: Some(key),
                kind: SymbolKind::TypeParameter,
            });
        }
        for item in &decl.members {
            if let Some((name, range, kind)) = item_binding(item, true) {
                let key = DefinitionKey::TopLevel {
                    uri: self.uri.clone(),
                    name: name.clone(),
                    container: Some(child_container.clone()),
                };
                self.add_scope_binding(
                    scope,
                    SymbolInfo {
                        name,
                        kind,
                        name_range: range,
                        full_range: range,
                        detail: None,
                        doc: None,
                        container: Some(child_container.clone()),
                        key,
                        scope,
                        children: Vec::new(),
                    },
                );
            }
        }
        self.walk_type(&decl.backing);
        self.walk_items_in(&decl.members, Some(&child_container));
        self.pop_scope();
    }

    fn walk_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Number(_) | ExprKind::Char(_) => {}
            ExprKind::String(parts) | ExprKind::MultilineString(parts) => {
                self.walk_str_parts(parts)
            }
            ExprKind::Tag { name, args } => {
                let resolved = self.resolve_tag(name);
                self.add_reference(name.clone(), resolved, SymbolKind::Tag);
                for arg in args {
                    self.walk_expr(arg);
                }
            }
            ExprKind::List(elements) | ExprKind::Tuple(elements) => {
                for element in elements {
                    self.walk_expr(element);
                }
            }
            ExprKind::Record { fields, update } => {
                if let Some(update) = update {
                    self.walk_expr(update);
                }
                let mut seen = HashSet::new();
                for field in fields {
                    if !seen.insert(field.name.text.clone()) {
                        self.diagnostics.push(AnalysisDiagnostic::warning(
                            field.name.range,
                            "duplicate-field",
                            format!("field `{}` is defined more than once", field.name.text),
                        ));
                    }
                    let resolved = if field.value.is_none() {
                        self.lookup_scopes(&field.name.text)
                    } else {
                        None
                    }
                    .or_else(|| {
                        self.resolver
                            .record_field_names
                            .get(&field.name.text)
                            .cloned()
                    });
                    self.add_reference(field.name.clone(), resolved, SymbolKind::Field);
                    if let Some(value) = &field.value {
                        self.walk_expr(value);
                    }
                    if let Some(default) = &field.default {
                        self.walk_expr(default);
                    }
                }
            }
            ExprKind::Lambda { params, body } => {
                self.push_scope(expr.range);
                for param in params {
                    self.bind_pattern(param, SymbolKind::Parameter);
                }
                self.walk_expr(body);
                self.pop_scope();
            }
            ExprKind::Block { stmts, tail } => {
                self.push_scope(expr.range);
                for stmt in stmts {
                    match &stmt.kind {
                        StmtKind::Definition(definition) => self.walk_local_definition(definition),
                        StmtKind::TypeDecl(decl) => {
                            let scope = self.scope_stack.last().copied().unwrap_or(0);
                            let key = DefinitionKey::Local {
                                uri: self.uri.clone(),
                                def_start: decl.name.range.start,
                            };
                            self.add_scope_binding(
                                scope,
                                SymbolInfo {
                                    name: decl.name.text.clone(),
                                    kind: SymbolKind::Type,
                                    name_range: decl.name.range,
                                    full_range: decl.range,
                                    detail: Some(slice(self.source, decl.backing.range)),
                                    doc: decl.doc.as_ref().map(|doc| doc.text.clone()),
                                    container: None,
                                    key: key.clone(),
                                    scope,
                                    children: Vec::new(),
                                },
                            );
                            self.references.push(Reference {
                                range: decl.name.range,
                                text: decl.name.text.clone(),
                                key: Some(key),
                                kind: SymbolKind::Type,
                            });
                            self.walk_type(&decl.backing);
                        }
                        StmtKind::Assignment { pattern, value } => {
                            self.walk_expr(value);
                            if let PatternKind::Name { name, .. } = &pattern.kind {
                                if name.text.starts_with('$') {
                                    if let Some(resolved) = self.lookup_scopes(&name.text) {
                                        self.add_reference(
                                            name.clone(),
                                            Some(resolved),
                                            SymbolKind::Variable,
                                        );
                                        self.walk_pattern_refs(pattern);
                                        continue;
                                    }
                                }
                            }
                            self.bind_pattern(pattern, SymbolKind::Local);
                        }
                        StmtKind::Db(value) | StmtKind::Expect(value) | StmtKind::Crash(value) => {
                            self.walk_expr(value)
                        }
                        StmtKind::Return(value) => {
                            if let Some(value) = value {
                                self.walk_expr(value);
                            }
                        }
                        StmtKind::Break | StmtKind::Continue => {}
                        StmtKind::Expr(value) => self.walk_expr(value),
                    }
                }
                if let Some(tail) = tail {
                    self.walk_expr(tail);
                }
                self.pop_scope();
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.walk_expr(condition);
                self.walk_expr(then_branch);
                self.walk_expr(else_branch);
            }
            ExprKind::Match { subject, branches } => {
                self.walk_expr(subject);
                for branch in branches {
                    self.push_scope(branch.range);
                    for pattern in &branch.patterns {
                        self.bind_pattern(pattern, SymbolKind::Local);
                    }
                    if let Some(guard) = &branch.guard {
                        self.walk_expr(guard);
                    }
                    self.walk_expr(&branch.body);
                    self.pop_scope();
                }
            }
            ExprKind::Lookup { path } => self.walk_lookup(path),
            ExprKind::Field { base, field, .. } => {
                self.walk_expr(base);
                self.add_reference(field.clone(), None, SymbolKind::Field);
            }
            ExprKind::TupleIndex { base, .. } => self.walk_expr(base),
            ExprKind::Call { callee, args } => {
                self.walk_expr(callee);
                for arg in args {
                    self.walk_expr(arg);
                }
            }
            ExprKind::MethodCall {
                receiver,
                method,
                args,
            } => {
                self.walk_expr(receiver);
                let resolved = match &receiver.kind {
                    ExprKind::Lookup { path }
                        if path
                            .last()
                            .is_some_and(|name| name.text == "self" || name.text == "Self") =>
                    {
                        self.lookup_scopes(&method.text)
                    }
                    _ => None,
                };
                self.add_reference(method.clone(), resolved, SymbolKind::Method);
                for arg in args {
                    self.walk_expr(arg);
                }
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.walk_expr(lhs);
                self.walk_expr(rhs);
            }
            ExprKind::Unary { expr, .. } => self.walk_expr(expr),
            ExprKind::Paren(inner) => self.walk_expr(inner),
            ExprKind::For {
                pattern,
                iterable,
                body,
            } => {
                self.walk_expr(iterable);
                self.push_scope(body.range);
                self.bind_pattern(pattern, SymbolKind::Local);
                self.walk_expr(body);
                self.pop_scope();
            }
            ExprKind::While { condition, body } => {
                self.walk_expr(condition);
                self.walk_expr(body);
            }
            ExprKind::Return(value) => {
                if let Some(value) = value {
                    self.walk_expr(value);
                }
            }
            ExprKind::Break | ExprKind::Continue | ExprKind::Error => {}
            ExprKind::Crash(value) | ExprKind::Db(value) => self.walk_expr(value),
            ExprKind::Nominal { type_name, value } => {
                self.walk_qualname(type_name, SymbolKind::Type, true);
                self.walk_expr(value);
            }
        }
    }

    fn walk_str_parts(&mut self, parts: &[StrPart]) {
        for part in parts {
            if let StrPartKind::Interpolation(expr) = &part.kind {
                self.walk_expr(expr);
            }
        }
    }

    fn walk_lookup(&mut self, path: &[Name]) {
        if path.is_empty() {
            return;
        }
        let result = self.resolve_path_names(path);
        for (index, name) in path.iter().enumerate() {
            let resolved = if index + 1 == path.len() {
                result.clone()
            } else {
                self.resolve_path_names(&path[..=index])
            };
            let fallback = if index == 0 {
                SymbolKind::Variable
            } else {
                SymbolKind::Field
            };
            self.add_reference(name.clone(), resolved, fallback);
        }
        if result.is_none() && path.len() > 1 && self.parsed_clean {
            self.report_unresolved(path);
        }
    }

    fn report_unresolved(&mut self, path: &[Name]) {
        let first = &path[0];
        let first_local = self.lookup_scopes(&first.text);
        let first_global = if first_local.is_none() {
            self.resolver.resolve_unqualified(&first.text)
        } else {
            None
        };
        match first_local.or(first_global) {
            None => {
                if self.reported.insert((first.range.start, first.range.end)) {
                    self.diagnostics.push(AnalysisDiagnostic::error(
                        first.range,
                        "unknown-module",
                        format!("could not resolve module `{}`", first.text),
                    ));
                }
            }
            Some(resolved) => {
                let diagnostic_kind = match resolved.kind {
                    SymbolKind::Import => {
                        if self.resolver.import_target(&first.text).is_none() {
                            return;
                        }
                        true
                    }
                    // Only modules and types provide qualified namespaces.
                    // `settings.greet` where `settings` is a runtime value may
                    // be valid even when the field shape is not statically
                    // known, so never invent an error for value qualifiers.
                    SymbolKind::Module | SymbolKind::Type => true,
                    _ => false,
                };
                if !diagnostic_kind {
                    return;
                }
                let last = path.last().unwrap();
                if self.reported.insert((last.range.start, last.range.end)) {
                    self.diagnostics.push(AnalysisDiagnostic::error(
                        last.range,
                        "unknown-name",
                        format!("`{}` is not exported by `{}`", last.text, first.text),
                    ));
                }
            }
        }
    }

    fn resolve_path_names(&mut self, path: &[Name]) -> Option<ResolvedName> {
        if path.is_empty() {
            return None;
        }
        let first = &path[0];
        if let Some(local) = self.lookup_scopes(&first.text) {
            if path.len() == 1 {
                return Some(local);
            }
            if let Some(target) = self.resolver.import_target(&first.text) {
                return resolve_in_summary(&target, &path[1..]);
            }
            let joined = join_names(path);
            if let Some(found) = self.resolver.type_members.get(&joined) {
                return Some(found.clone());
            }
            return None;
        }
        self.resolver.resolve_path(path)
    }

    fn walk_qualname(&mut self, qual: &QualName, hint: SymbolKind, diagnose: bool) {
        let path = &qual.parts;
        if path.is_empty() {
            return;
        }
        let result = self.resolve_path_names(path);
        for (index, name) in path.iter().enumerate() {
            let resolved = if index + 1 == path.len() {
                result.clone()
            } else {
                self.resolve_path_names(&path[..=index])
            };
            let kind = resolved.as_ref().map(|found| found.kind).unwrap_or(hint);
            self.add_reference(name.clone(), resolved, kind);
        }
        if diagnose && result.is_none() && path.len() > 1 && self.parsed_clean {
            let first = &path[0];
            let unresolved = self.lookup_scopes(&first.text).is_none()
                && self.resolver.resolve_unqualified(&first.text).is_none();
            if unresolved && self.reported.insert((first.range.start, first.range.end)) {
                self.diagnostics.push(AnalysisDiagnostic::error(
                    first.range,
                    "unknown-module",
                    format!("could not resolve module `{}`", first.text),
                ));
            }
        }
    }

    fn walk_type(&mut self, ty: &TypeExpr) {
        match &ty.kind {
            TypeKind::Named(qual) => self.walk_qualname(qual, SymbolKind::Type, false),
            TypeKind::Var(name) => {
                let resolved = self.lookup_scopes(&name.text);
                self.add_reference(name.clone(), resolved, SymbolKind::TypeParameter);
            }
            TypeKind::Wildcard | TypeKind::Error => {}
            TypeKind::Function { args, ret, .. } => {
                for arg in args {
                    self.walk_type(arg);
                }
                self.walk_type(ret);
            }
            TypeKind::Tuple(types) => {
                for ty in types {
                    self.walk_type(ty);
                }
            }
            TypeKind::Record { fields, open } => {
                let mut seen = HashSet::new();
                for field in fields {
                    if !seen.insert(field.name.text.clone()) {
                        self.diagnostics.push(AnalysisDiagnostic::warning(
                            field.name.range,
                            "duplicate-field",
                            format!("field `{}` is defined more than once", field.name.text),
                        ));
                    }
                    let resolved = self
                        .resolver
                        .record_fields
                        .get(&field.name.range.start)
                        .cloned()
                        .or_else(|| {
                            self.resolver
                                .record_field_names
                                .get(&field.name.text)
                                .cloned()
                        });
                    self.add_reference(field.name.clone(), resolved, SymbolKind::Field);
                    self.walk_type(&field.ty);
                    if let Some(default) = &field.default {
                        self.walk_expr(default);
                    }
                }
                if let Some(Some(name)) = open {
                    self.add_reference(name.clone(), None, SymbolKind::Local);
                }
            }
            TypeKind::TagUnion { tags, open } => {
                for tag in tags {
                    let resolved = self.resolve_tag(&tag.name);
                    self.add_reference(tag.name.clone(), resolved, SymbolKind::Tag);
                    for payload in &tag.payloads {
                        self.walk_type(payload);
                    }
                }
                if let Some(Some(name)) = open {
                    self.add_reference(name.clone(), None, SymbolKind::Local);
                }
            }
            TypeKind::Apply { name, args } => {
                self.walk_qualname(name, SymbolKind::Type, false);
                for arg in args {
                    self.walk_type(arg);
                }
            }
            TypeKind::Where { base, requirements } => {
                self.walk_type(base);
                for requirement in requirements {
                    let resolved = self.lookup_scopes(&requirement.ty_var.text);
                    self.add_reference(
                        requirement.ty_var.clone(),
                        resolved,
                        SymbolKind::TypeParameter,
                    );
                    self.add_reference(requirement.method.clone(), None, SymbolKind::Method);
                    self.walk_type(&requirement.ty);
                }
            }
        }
    }

    fn bind_pattern(&mut self, pattern: &Pattern, kind: SymbolKind) {
        let scope = self.scope_stack.last().copied().unwrap_or(0);
        let mut bindings = Vec::new();
        pattern.bindings(&mut bindings);
        for (name, range, is_var) in bindings {
            if name == "_" {
                continue;
            }
            let binding_kind = if kind == SymbolKind::Parameter {
                SymbolKind::Parameter
            } else if is_var {
                SymbolKind::Variable
            } else {
                SymbolKind::Local
            };
            let key = DefinitionKey::Local {
                uri: self.uri.clone(),
                def_start: range.start,
            };
            self.add_scope_binding(
                scope,
                SymbolInfo {
                    name: name.clone(),
                    kind: binding_kind,
                    name_range: range,
                    full_range: range,
                    detail: None,
                    doc: None,
                    container: None,
                    key: key.clone(),
                    scope,
                    children: Vec::new(),
                },
            );
            self.references.push(Reference {
                range,
                text: name,
                key: Some(key),
                kind: binding_kind,
            });
        }
        self.walk_pattern_refs(pattern);
    }

    fn walk_pattern_refs(&mut self, pattern: &Pattern) {
        match &pattern.kind {
            PatternKind::Tag {
                qualifier,
                name,
                args,
            } => {
                let resolved = if let Some(qualifier) = qualifier {
                    self.walk_qualname(qualifier, SymbolKind::Type, false);
                    let mut path = qualifier.parts.clone();
                    path.push(name.clone());
                    let resolved = self.resolve_path_names(&path);
                    if resolved.is_none() {
                        self.report_unresolved(&path);
                    }
                    resolved
                } else {
                    self.resolve_tag(name)
                };
                self.add_reference(name.clone(), resolved, SymbolKind::Tag);
                for arg in args {
                    self.walk_pattern_refs(arg);
                }
            }
            PatternKind::Record { fields, .. } => {
                for field in fields {
                    self.add_reference(field.name.clone(), None, SymbolKind::Field);
                    if let Some(pattern) = &field.pattern {
                        self.walk_pattern_refs(pattern);
                    }
                }
            }
            PatternKind::String(parts) => self.walk_str_parts(parts),
            PatternKind::Tuple(elements)
            | PatternKind::Alternatives(elements)
            | PatternKind::List { elements, rest: _ } => {
                for element in elements {
                    self.walk_pattern_refs(element);
                }
            }
            PatternKind::As { pattern, .. } => self.walk_pattern_refs(pattern),
            PatternKind::Nominal { type_name, pattern } => {
                self.walk_qualname(type_name, SymbolKind::Type, true);
                self.walk_pattern_refs(pattern);
            }
            PatternKind::Name { .. }
            | PatternKind::Wildcard
            | PatternKind::Number(_)
            | PatternKind::Char(_)
            | PatternKind::Error => {}
        }
    }

    fn resolve_tag(&mut self, name: &Name) -> Option<ResolvedName> {
        if let Some(local) = self.lookup_scopes(&name.text) {
            return Some(local);
        }
        self.resolver.resolve_tag(&name.text)
    }

    fn lookup_scopes(&self, name: &str) -> Option<ResolvedName> {
        for scope in self.scope_stack.iter().rev() {
            let scope = self.scopes.get(*scope as usize)?;
            if let Some(binding) = scope.bindings.iter().find(|binding| binding.name == name) {
                return Some(ResolvedName {
                    key: binding.key.clone(),
                    kind: binding.kind,
                });
            }
        }
        None
    }

    fn add_reference(&mut self, name: Name, resolved: Option<ResolvedName>, fallback: SymbolKind) {
        let (key, kind) = match resolved {
            Some(resolved) => (Some(resolved.key), resolved.kind),
            None => (None, fallback),
        };
        self.references.push(Reference {
            range: name.range,
            text: name.text,
            key,
            kind,
        });
    }

    fn add_scope_binding(&mut self, scope: u32, binding: SymbolInfo) {
        if let Some(info) = self.scopes.get_mut(scope as usize) {
            if let Some(index) = info
                .bindings
                .iter()
                .position(|existing| existing.name == binding.name)
            {
                info.bindings[index] = binding;
            } else {
                info.bindings.push(binding);
            }
        }
    }

    fn push_scope(&mut self, range: TextRange) -> u32 {
        let id = self.scopes.len() as u32;
        let parent = self.scope_stack.last().copied();
        self.scopes.push(ScopeInfo {
            id,
            range,
            parent,
            bindings: Vec::new(),
        });
        self.scope_stack.push(id);
        id
    }

    fn pop_scope(&mut self) {
        if self.scope_stack.len() > 1 {
            self.scope_stack.pop();
        }
    }
}

fn item_binding(item: &Item, in_type: bool) -> Option<(String, TextRange, SymbolKind)> {
    match &item.kind {
        ItemKind::Definition(definition) => definition.simple_name().map(|name| {
            (
                name.text.clone(),
                name.range,
                definition_kind(definition, in_type),
            )
        }),
        ItemKind::Annotation(annotation) => Some((
            annotation.name.text.clone(),
            annotation.name.range,
            if is_function_type(&annotation.ty) {
                SymbolKind::Function
            } else {
                SymbolKind::Constant
            },
        )),
        ItemKind::TypeDecl(decl) => {
            Some((decl.name.text.clone(), decl.name.range, SymbolKind::Type))
        }
        ItemKind::WhereAlias(alias) => {
            Some((alias.name.text.clone(), alias.name.range, SymbolKind::Type))
        }
        _ => None,
    }
}

/* ------------------------------------------------------------------------- */
/* Signatures                                                                */
/* ------------------------------------------------------------------------- */

fn collect_signatures(
    uri: &Url,
    source: &str,
    module: &Module,
) -> HashMap<DefinitionKey, Signature> {
    let mut output = HashMap::new();
    collect_item_signatures(uri, source, &module.items, None, &mut output);
    output
}

fn collect_item_signatures(
    uri: &Url,
    source: &str,
    items: &[Item],
    container: Option<&str>,
    output: &mut HashMap<DefinitionKey, Signature>,
) {
    for item in items {
        match &item.kind {
            ItemKind::Definition(definition) => {
                if let Some(name) = definition.simple_name() {
                    let key = DefinitionKey::TopLevel {
                        uri: uri.clone(),
                        name: name.text.clone(),
                        container: container.map(str::to_string),
                    };
                    if let Some(signature) = definition_signature(definition, &key, source) {
                        output.insert(key, signature);
                    }
                }
            }
            ItemKind::Annotation(annotation) => {
                let key = DefinitionKey::TopLevel {
                    uri: uri.clone(),
                    name: annotation.name.text.clone(),
                    container: container.map(str::to_string),
                };
                output.insert(
                    key.clone(),
                    signature_from_type(
                        &annotation.name.text,
                        &annotation.ty,
                        source,
                        annotation
                            .doc
                            .as_ref()
                            .map(|doc| doc.text.trim().to_string()),
                        key,
                    ),
                );
            }
            ItemKind::TypeDecl(decl) => {
                let child = match container {
                    Some(container) => format!("{container}.{}", decl.name.text),
                    None => decl.name.text.clone(),
                };
                collect_item_signatures(uri, source, &decl.members, Some(&child), output);
            }
            _ => {}
        }
    }
}

fn definition_signature(
    definition: &Definition,
    key: &DefinitionKey,
    source: &str,
) -> Option<Signature> {
    let name = definition.simple_name()?.text.clone();
    let doc = definition
        .doc
        .as_ref()
        .map(|doc| doc.text.trim().to_string());
    if let Some(annotation) = &definition.annotation {
        return Some(signature_from_type(
            &name,
            annotation,
            source,
            doc,
            key.clone(),
        ));
    }
    if let ExprKind::Lambda { params, body } = &definition.value.kind {
        let mut out = Vec::new();
        for (index, param) in params.iter().enumerate() {
            let mut bindings = Vec::new();
            param.bindings(&mut bindings);
            if let Some((name, range, _)) = bindings.first() {
                out.push(ParamInfo {
                    name: name.clone(),
                    ty: None,
                    name_range: *range,
                });
            } else {
                out.push(ParamInfo {
                    name: format!("arg{}", index + 1),
                    ty: None,
                    name_range: param.range,
                });
            }
        }
        return Some(Signature {
            name: name.clone(),
            params: out,
            return_type: None,
            display: format!("{name} : _"),
            is_effectful: expr_has_effect(body),
            type_params: Vec::new(),
            doc,
            key: key.clone(),
        });
    }
    None
}

fn signature_from_type(
    name: &str,
    ty: &TypeExpr,
    source: &str,
    doc: Option<String>,
    key: DefinitionKey,
) -> Signature {
    let annotation_text = slice(source, ty.range);
    let mut type_params = Vec::new();
    collect_type_vars(ty, &mut type_params);
    if let TypeKind::Function {
        args,
        ret,
        effectful,
    } = &ty.kind
    {
        let params = args
            .iter()
            .enumerate()
            .map(|(index, arg)| ParamInfo {
                name: format!("arg{}", index + 1),
                ty: Some(slice(source, arg.range)),
                name_range: arg.range,
            })
            .collect();
        Signature {
            name: name.to_string(),
            params,
            return_type: Some(slice(source, ret.range)),
            display: format!("{name} : {annotation_text}"),
            is_effectful: *effectful,
            type_params,
            doc,
            key,
        }
    } else {
        Signature {
            name: name.to_string(),
            params: Vec::new(),
            return_type: Some(annotation_text.clone()),
            display: format!("{name} : {annotation_text}"),
            is_effectful: false,
            type_params,
            doc,
            key,
        }
    }
}

fn collect_type_vars(ty: &TypeExpr, output: &mut Vec<String>) {
    match &ty.kind {
        TypeKind::Var(name) => {
            if !output.contains(&name.text) {
                output.push(name.text.clone());
            }
        }
        TypeKind::Function { args, ret, .. } => {
            for arg in args {
                collect_type_vars(arg, output);
            }
            collect_type_vars(ret, output);
        }
        TypeKind::Tuple(types) => {
            for ty in types {
                collect_type_vars(ty, output);
            }
        }
        TypeKind::Record { fields, .. } => {
            for field in fields {
                collect_type_vars(&field.ty, output);
            }
        }
        TypeKind::TagUnion { tags, .. } => {
            for tag in tags {
                for payload in &tag.payloads {
                    collect_type_vars(payload, output);
                }
            }
        }
        TypeKind::Apply { args, .. } => {
            for arg in args {
                collect_type_vars(arg, output);
            }
        }
        TypeKind::Where { base, requirements } => {
            collect_type_vars(base, output);
            for requirement in requirements {
                collect_type_vars(&requirement.ty, output);
            }
        }
        _ => {}
    }
}

fn add_builtin_signatures(
    signatures: &mut HashMap<DefinitionKey, Signature>,
    references: &[Reference],
    builtins: &BuiltinIndex,
) {
    let mut seen = HashSet::new();
    for reference in references {
        let Some(DefinitionKey::Builtin { full_name }) = &reference.key else {
            continue;
        };
        if !seen.insert(full_name.clone()) {
            continue;
        }
        let Some(item) = builtins.lookup(full_name) else {
            continue;
        };
        let key = item.key();
        if signatures.contains_key(&key) {
            continue;
        }
        signatures.insert(key.clone(), builtin_signature(item, key));
    }
}

fn builtin_signature(item: &super::BuiltinItem, key: DefinitionKey) -> Signature {
    let mut params = Vec::new();
    let mut return_type = None;
    if let Some(text) = &item.signature {
        if let Some((args, ret)) = split_signature(text) {
            for (index, arg) in args.iter().enumerate() {
                params.push(ParamInfo {
                    name: format!("arg{}", index + 1),
                    ty: Some(arg.clone()),
                    name_range: TextRange::default(),
                });
            }
            return_type = Some(ret);
        }
    }
    let display = item
        .signature
        .as_ref()
        .map(|signature| format!("{} : {}", item.name, signature))
        .unwrap_or_else(|| item.name.clone());
    Signature {
        name: item.name.clone(),
        params,
        return_type,
        display,
        is_effectful: item
            .signature
            .as_deref()
            .is_some_and(|signature| signature.contains("=>")),
        type_params: Vec::new(),
        doc: item.doc.clone(),
        key,
    }
}

fn split_signature(text: &str) -> Option<(Vec<String>, String)> {
    let arrow = top_level_arrow(text)?;
    let args = split_top_level(&text[..arrow], ',');
    let ret = text[arrow + 2..].trim().to_string();
    Some((args, ret))
}

fn top_level_arrow(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut index = 0;
    while index + 1 < bytes.len() {
        match bytes[index] {
            b'(' | b'{' | b'[' => depth += 1,
            b')' | b'}' | b']' => depth -= 1,
            b'-' if bytes[index + 1] == b'>' && depth == 0 => return Some(index),
            _ => {}
        }
        index += 1;
    }
    None
}

fn split_top_level(text: &str, separator: char) -> Vec<String> {
    let mut output = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (index, ch) in text.char_indices() {
        match ch {
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => depth -= 1,
            _ if ch == separator && depth == 0 => {
                output.push(text[start..index].trim().to_string());
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        output.push(tail.to_string());
    }
    output
}

/* ------------------------------------------------------------------------- */
/* Utilities                                                                 */
/* ------------------------------------------------------------------------- */

fn slice(source: &str, range: TextRange) -> String {
    source
        .get(range.start as usize..range.end as usize)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn expr_has_effect(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Lookup { path } => path.last().is_some_and(|name| name.text.ends_with('!')),
        ExprKind::MethodCall {
            receiver,
            method,
            args,
        } => {
            method.text.ends_with('!')
                || expr_has_effect(receiver)
                || args.iter().any(expr_has_effect)
        }
        ExprKind::Call { callee, args } => {
            expr_has_effect(callee) || args.iter().any(expr_has_effect)
        }
        ExprKind::Number(_) | ExprKind::Char(_) => false,
        ExprKind::Tag { name, .. } if name.text.ends_with('!') => true,
        ExprKind::Tag { args, .. } => args.iter().any(expr_has_effect),
        ExprKind::List(elements) | ExprKind::Tuple(elements) => {
            elements.iter().any(expr_has_effect)
        }
        ExprKind::Record { fields, update } => {
            update.as_deref().is_some_and(expr_has_effect)
                || fields.iter().any(|field| {
                    field.value.as_ref().is_some_and(expr_has_effect)
                        || field.default.as_ref().is_some_and(expr_has_effect)
                })
        }
        ExprKind::Lambda { body, .. } => expr_has_effect(body),
        ExprKind::Block { stmts, tail } => {
            stmts.iter().any(|stmt| match &stmt.kind {
                StmtKind::Definition(definition) => expr_has_effect(&definition.value),
                StmtKind::TypeDecl(_) => false,
                StmtKind::Assignment { value, .. } => expr_has_effect(value),
                StmtKind::Db(value) | StmtKind::Expect(value) | StmtKind::Crash(value) => {
                    expr_has_effect(value)
                }
                StmtKind::Return(Some(value)) => expr_has_effect(value),
                StmtKind::Expr(value) => expr_has_effect(value),
                _ => false,
            }) || tail.as_deref().is_some_and(expr_has_effect)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_has_effect(condition)
                || expr_has_effect(then_branch)
                || expr_has_effect(else_branch)
        }
        ExprKind::Match { subject, branches } => {
            expr_has_effect(subject)
                || branches.iter().any(|branch| {
                    branch.guard.as_ref().is_some_and(expr_has_effect)
                        || expr_has_effect(&branch.body)
                })
        }
        ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => expr_has_effect(base),
        ExprKind::Binary { lhs, rhs, .. } => expr_has_effect(lhs) || expr_has_effect(rhs),
        ExprKind::Unary { expr, .. } => expr_has_effect(expr),
        ExprKind::Paren(inner) => expr_has_effect(inner),
        ExprKind::For { iterable, body, .. } => expr_has_effect(iterable) || expr_has_effect(body),
        ExprKind::While { condition, body } => expr_has_effect(condition) || expr_has_effect(body),
        ExprKind::Return(value) => value.as_deref().is_some_and(expr_has_effect),
        ExprKind::Crash(value) | ExprKind::Db(value) => expr_has_effect(value),
        ExprKind::Nominal { value, .. } => expr_has_effect(value),
        ExprKind::String(parts) | ExprKind::MultilineString(parts) => parts.iter().any(
            |part| matches!(&part.kind, StrPartKind::Interpolation(expr) if expr_has_effect(expr)),
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::builtins::{BuiltinDatabase, BuiltinItem, BuiltinModule};
    use crate::syntax::ast::{Doc, TypeDeclKind};
    use crate::syntax::token::Token;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn range(start: u32, end: u32) -> TextRange {
        TextRange::new(start, end)
    }

    fn name(text: &str, start: u32) -> Name {
        Name::new(text, range(start, start + text.len() as u32))
    }

    fn number(start: u32, end: u32) -> Expr {
        Expr::new(ExprKind::Number("1".to_string()), range(start, end))
    }

    fn lookup(start: u32, parts: &[(&str, u32)]) -> Expr {
        let path = parts
            .iter()
            .map(|(text, position)| name(text, *position))
            .collect();
        let end = parts
            .last()
            .map(|(text, position)| position + text.len() as u32)
            .unwrap_or(start);
        Expr::new(ExprKind::Lookup { path }, range(start, end))
    }

    fn definition(
        pattern: Pattern,
        value: Expr,
        annotation: Option<TypeExpr>,
        start: u32,
        end: u32,
    ) -> Item {
        Item {
            kind: ItemKind::Definition(Definition {
                pattern,
                annotation,
                value,
                is_var: false,
                doc: None,
                range: range(start, end),
                equals_range: range(start, start + 1),
            }),
            range: range(start, end),
        }
    }

    fn simple_pattern(text: &str, start: u32) -> Pattern {
        Pattern::new(
            PatternKind::Name {
                name: name(text, start),
                is_var: false,
            },
            range(start, start + text.len() as u32),
        )
    }

    fn module(items: Vec<Item>, imports: Vec<Import>) -> Module {
        Module {
            header: None,
            imports,
            items,
            range: range(0, 100),
        }
    }

    fn analyze_module(
        uri: Url,
        text: &str,
        module: Module,
        workspace: &WorkspaceIndex,
        builtins: &BuiltinIndex,
    ) -> AnalyzedFile {
        let source = Arc::new(SourceFile::new(uri.clone(), text.to_string()));
        let parsed = Arc::new(ParsedFile {
            module,
            tokens: Vec::<Token>::new(),
            diagnostics: Vec::new(),
        });
        analyze(
            uri,
            source,
            parsed,
            workspace,
            builtins,
            &AnalysisConfig::default(),
        )
    }

    #[test]
    fn shadowing_prefers_innermost_binding() {
        let uri = Url::parse("file:///tmp/main.roc").unwrap();
        let text = "x = 1\nx = 2\nx = 3\n";
        let inner = definition(simple_pattern("x", 8), number(12, 13), None, 8, 15);
        let block = Expr::new(
            ExprKind::Block {
                stmts: vec![crate::syntax::ast::Stmt {
                    kind: StmtKind::Definition(Definition {
                        pattern: simple_pattern("x", 8),
                        annotation: None,
                        value: number(12, 13),
                        is_var: false,
                        doc: None,
                        range: range(8, 15),
                        equals_range: range(10, 11),
                    }),
                    range: range(8, 15),
                }],
                tail: Some(Box::new(lookup(14, &[("x", 14)]))),
            },
            range(6, 17),
        );
        let items = vec![
            definition(simple_pattern("x", 0), number(4, 5), None, 0, 5),
            definition(simple_pattern("y", 6), block, None, 6, 17),
        ];
        let workspace = WorkspaceIndex::empty(PathBuf::from("/tmp"));
        let analysis = analyze_module(
            uri.clone(),
            text,
            module(items, Vec::new()),
            &workspace,
            &BuiltinIndex::empty(),
        );
        let reference = analysis.reference_at(14).expect("binding use reference");
        assert_eq!(
            reference.key,
            Some(DefinitionKey::Local { uri, def_start: 8 })
        );
        let visible = analysis.visible_bindings(14);
        assert_eq!(
            visible.first().map(|symbol| symbol.name.as_str()),
            Some("x")
        );
        assert_eq!(
            visible.first().map(|symbol| &symbol.key),
            Some(&DefinitionKey::Local {
                uri: Url::parse("file:///tmp/main.roc").unwrap(),
                def_start: 8
            })
        );
        let _ = inner;
    }

    #[test]
    fn resolves_builtin_qualified_name_and_signature() {
        let uri = Url::parse("file:///tmp/main.roc").unwrap();
        let text = "x = Str.concat\n";
        let items = vec![definition(
            simple_pattern("x", 0),
            lookup(4, &[("Str", 4), ("concat", 8)]),
            None,
            0,
            14,
        )];
        let database = BuiltinDatabase {
            version: "test".to_string(),
            modules: vec![BuiltinModule {
                name: "Str".to_string(),
                doc: None,
                items: vec![BuiltinItem {
                    name: "Str.concat".to_string(),
                    module: "Str".to_string(),
                    short_name: "concat".to_string(),
                    kind: crate::analysis::builtins::BuiltinKind::Value,
                    signature: Some("Str, Str -> Str".to_string()),
                    doc: Some("Concatenate.".to_string()),
                    type_def: None,
                }],
            }],
        };
        let builtins = BuiltinIndex::from_database(database);
        let workspace = WorkspaceIndex::empty(PathBuf::from("/tmp"));
        let analysis = analyze_module(uri, text, module(items, Vec::new()), &workspace, &builtins);
        let reference = analysis
            .references
            .iter()
            .find(|reference| reference.text == "concat")
            .expect("concat reference");
        let key = DefinitionKey::Builtin {
            full_name: "Str.concat".to_string(),
        };
        assert_eq!(reference.key, Some(key.clone()));
        let signature = analysis.signature_for(&key).expect("builtin signature");
        assert_eq!(signature.params.len(), 2);
        assert_eq!(signature.return_type.as_deref(), Some("Str"));
    }

    #[test]
    fn resolves_qualified_builtin_tags() {
        let uri = Url::parse("file:///tmp/main.roc").unwrap();
        let text = "x = Bool.True\n";
        let items = vec![definition(
            simple_pattern("x", 0),
            lookup(4, &[("Bool", 4), ("True", 9)]),
            None,
            0,
            13,
        )];
        let database = BuiltinDatabase {
            version: "test".to_string(),
            modules: vec![BuiltinModule {
                name: "Bool".to_string(),
                doc: None,
                items: vec![BuiltinItem {
                    name: "Bool".to_string(),
                    module: "Bool".to_string(),
                    short_name: "Bool".to_string(),
                    kind: crate::analysis::builtins::BuiltinKind::Type,
                    signature: None,
                    doc: None,
                    type_def: Some(":= [False, True]".to_string()),
                }],
            }],
        };
        let builtins = BuiltinIndex::from_database(database);
        let workspace = WorkspaceIndex::empty(PathBuf::from("/tmp"));
        let analysis = analyze_module(uri, text, module(items, Vec::new()), &workspace, &builtins);
        assert!(
            analysis.diagnostics.is_empty(),
            "{:?}",
            analysis.diagnostics
        );
        let reference = analysis
            .references
            .iter()
            .find(|reference| reference.text == "True")
            .expect("True reference");
        assert_eq!(
            reference.key,
            Some(DefinitionKey::Builtin {
                full_name: "Bool.True".to_string(),
            })
        );
        assert_eq!(reference.kind, SymbolKind::Tag);
    }

    #[test]
    fn unknown_fields_on_values_do_not_produce_false_errors() {
        let text = "default_settings = || { greet: \"hi\" }\nsettings = default_settings()\nrecord_greeting = settings.greet\n";
        let uri = Url::parse("file:///tmp/main.roc").unwrap();
        let analysis = analyze_module(
            uri,
            text,
            crate::syntax::parse(text).module,
            &WorkspaceIndex::empty(PathBuf::from("/tmp")),
            &BuiltinIndex::empty(),
        );
        assert!(
            !analysis
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("is not exported")),
            "{:?}",
            analysis.diagnostics
        );
        let field = analysis
            .references
            .iter()
            .find(|reference| reference.text == "greet")
            .expect("greet reference");
        assert!(field.key.is_none() || field.kind == SymbolKind::Field);
    }

    fn color_summary() -> (ModuleSummary, DefinitionKey) {
        let uri = Url::parse("file:///tmp/Color.roc").unwrap();
        let method_key = DefinitionKey::TopLevel {
            uri: uri.clone(),
            name: "to_str".to_string(),
            container: Some("Color".to_string()),
        };
        let type_key = DefinitionKey::TopLevel {
            uri: uri.clone(),
            name: "Color".to_string(),
            container: None,
        };
        let export = Export {
            name: "Color".to_string(),
            kind: SymbolKind::Type,
            detail: None,
            doc: None,
            selection_range: range(0, 5),
            full_range: range(0, 20),
            container: None,
            key: Some(type_key),
            children: vec![Export {
                name: "to_str".to_string(),
                kind: SymbolKind::Method,
                detail: None,
                doc: None,
                selection_range: range(10, 16),
                full_range: range(10, 20),
                container: Some("Color".to_string()),
                key: Some(method_key.clone()),
                children: Vec::new(),
            }],
        };
        let summary = ModuleSummary {
            uri: uri.clone(),
            path: PathBuf::from("/tmp/Color.roc"),
            name: "Color".to_string(),
            kind: ModuleKind::TypeModule,
            exports: vec![export],
            dependencies: Vec::new(),
            header_kind: None,
            header_range: None,
            doc: None,
            package: None,
            package_root: None,
        };
        (summary, method_key)
    }

    #[test]
    fn resolves_imported_type_module_members_and_exposing() {
        let uri = Url::parse("file:///tmp/main.roc").unwrap();
        let text = "import Color exposing [to_str]\nx = Color.to_str\ny = to_str\n";
        let import = Import {
            path: ImportPath::Roc {
                raw: "Color".to_string(),
                segments: vec![name("Color", 7)],
                range: range(7, 12),
            },
            alias: None,
            ty: None,
            exposing: vec![name("to_str", 23)],
            range: range(0, 30),
        };
        let items = vec![
            definition(
                simple_pattern("x", 31),
                lookup(35, &[("Color", 35), ("to_str", 41)]),
                None,
                31,
                47,
            ),
            definition(
                simple_pattern("y", 48),
                lookup(52, &[("to_str", 52)]),
                None,
                48,
                58,
            ),
        ];
        let (summary, method_key) = color_summary();
        let workspace = WorkspaceIndex::new(PathBuf::from("/tmp"), None, vec![summary], Vec::new());
        let analysis = analyze_module(
            uri,
            text,
            module(items, vec![import]),
            &workspace,
            &BuiltinIndex::empty(),
        );
        assert_eq!(analysis.imports.len(), 1);
        assert_eq!(analysis.imports[0].qualifier, "Color");
        assert!(analysis.imports[0].target.is_some());
        let matches = analysis
            .references
            .iter()
            .filter(|reference| reference.text == "to_str")
            .collect::<Vec<_>>();
        assert!(matches.len() >= 2, "expected qualified and exposed uses");
        assert!(matches
            .iter()
            .all(|reference| reference.key.as_ref() == Some(&method_key)));
    }

    #[test]
    fn resolves_fake_url_cache_package() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().expect("temp dir");
        let cache = temp.path().join("cache");
        let hash = "FakeHash111";
        let package_dir = cache.join("packages").join(hash);
        std::fs::create_dir_all(&package_dir).expect("package dir");
        std::fs::write(package_dir.join("main.roc"), "package [Color] {}\n").expect("package root");
        std::fs::write(
            package_dir.join("Color.roc"),
            "Color :: [].{\n\tto_str : Color -> Str\n\tto_str = |_| \"\"\n}\n",
        )
        .expect("module");
        let nested_hash = "NestedHash222";
        let nested_dir = cache.join("packages").join(nested_hash);
        std::fs::create_dir_all(&nested_dir).expect("nested package dir");
        std::fs::write(nested_dir.join("main.roc"), "package [Nested] {}\n").expect("nested root");
        std::fs::write(
            nested_dir.join("Nested.roc"),
            "module [value]\n\nvalue = 1\n",
        )
        .expect("nested module");
        let spec = format!("https://example.com/{nested_hash}.tar.zst");
        let sidecar = format!(
            "roc-package-deps 5\nkind package\nexpanded_bytes 1\ncontent_bytes 1\nroot_source_hash {}\ndeps 1\ndep 0 - 6:nested {}:{}\n",
            "0".repeat(64),
            spec.len(),
            spec
        );
        std::fs::write(cache.join("packages").join(format!("{hash}.deps")), sidecar)
            .expect("sidecar");
        let workspace_dir = temp.path().join("ws");
        std::fs::create_dir_all(&workspace_dir).expect("workspace");
        std::fs::create_dir_all(workspace_dir.join("pkg")).expect("path package");
        std::fs::write(
            workspace_dir.join("pkg/main.roc"),
            "package [Greeting] {}\n",
        )
        .expect("path package root");
        std::fs::write(
            workspace_dir.join("pkg/Greeting.roc"),
            "Greeting :: [].{\n\thello : Str -> Str\n\thello = |_| \"\"\n}\n",
        )
        .expect("path package module");
        let main_text = format!(
            "app [main!] {{\n\tpalette: \"https://example.com/{hash}.tar.br\",\n\tlocal: \"pkg/main.roc\",\n\troc: \"nightly\",\n}}\n\nimport palette.Color\nimport local.Greeting\n\nmain! = |_| Color.to_str(Color)\n\nvalue = Greeting.hello(\"x\")\n"
        );
        std::fs::write(workspace_dir.join("main.roc"), &main_text).expect("app");
        let previous = std::env::var_os("ROC_CACHE_DIR");
        std::env::set_var("ROC_CACHE_DIR", &cache);
        let workspace = WorkspaceIndex::build(&workspace_dir, &AnalysisConfig::default());
        assert!(
            workspace.find_module(Some("palette"), "Color").is_some(),
            "package module should be indexed"
        );
        assert!(
            workspace.find_module(Some("local"), "Greeting").is_some(),
            "path package module should use its package-relative name"
        );
        assert!(
            workspace.find_module(Some("nested"), "Nested").is_some(),
            "sidecar dependencies should be indexed"
        );
        let uri = Url::from_file_path(workspace_dir.join("main.roc")).unwrap();
        let source = Arc::new(SourceFile::new(uri.clone(), main_text.clone()));
        let parsed = Arc::new(crate::syntax::parse(&main_text));
        let analysis = analyze(
            uri,
            source,
            parsed,
            &workspace,
            &BuiltinIndex::empty(),
            &AnalysisConfig::default(),
        );
        let reference = analysis
            .references
            .iter()
            .find(|reference| reference.text == "to_str")
            .expect("package member reference");
        assert!(matches!(
            &reference.key,
            Some(DefinitionKey::TopLevel { uri, .. }) if uri.path().ends_with("Color.roc")
        ));
        let path_reference = analysis
            .references
            .iter()
            .find(|reference| reference.text == "hello")
            .expect("path package member reference");
        assert!(matches!(
            &path_reference.key,
            Some(DefinitionKey::TopLevel { uri, .. }) if uri.path().ends_with("Greeting.roc")
        ));
        match previous {
            Some(value) => std::env::set_var("ROC_CACHE_DIR", value),
            None => std::env::remove_var("ROC_CACHE_DIR"),
        }
    }

    #[test]
    fn type_decl_exports_associated_methods() {
        let uri = Url::parse("file:///tmp/T.roc").unwrap();
        let decl = TypeDecl {
            name: name("T", 0),
            kind: TypeDeclKind::Nominal,
            params: Vec::new(),
            backing: TypeExpr::new(TypeKind::Wildcard, range(5, 6)),
            members: vec![definition(
                simple_pattern("method", 10),
                number(20, 21),
                None,
                10,
                21,
            )],
            doc: Some(Doc {
                text: " Documentation ".to_string(),
                range: range(0, 1),
            }),
            range: range(0, 22),
            operator_range: range(2, 4),
        };
        let items = vec![Item {
            kind: ItemKind::TypeDecl(decl),
            range: range(0, 22),
        }];
        let workspace = WorkspaceIndex::empty(PathBuf::from("/tmp"));
        let analysis = analyze_module(
            uri.clone(),
            "T :: []",
            module(items, Vec::new()),
            &workspace,
            &BuiltinIndex::empty(),
        );
        let symbol = &analysis.symbols[0];
        assert_eq!(symbol.kind, SymbolKind::Type);
        assert_eq!(symbol.children.len(), 1);
        assert_eq!(symbol.children[0].name, "method");
        assert_eq!(symbol.children[0].kind, SymbolKind::Method);
        assert_eq!(
            symbol.children[0].key,
            DefinitionKey::TopLevel {
                uri,
                name: "method".to_string(),
                container: Some("T".to_string()),
            }
        );
    }
}

#[cfg(test)]
mod followup_resolution_tests {
    use super::*;
    use crate::syntax::parse;
    use std::path::PathBuf;

    fn analyze_text(text: &str) -> AnalyzedFile {
        let uri = Url::parse("file:///tmp/main.roc").unwrap();
        let parsed = parse(text);
        let workspace = WorkspaceIndex::empty(PathBuf::from("/tmp"));
        let source = Arc::new(SourceFile::new(uri.clone(), text.to_string()));
        analyze(
            uri,
            source,
            Arc::new(parsed),
            &workspace,
            &BuiltinIndex::empty(),
            &AnalysisConfig::default(),
        )
    }

    #[test]
    fn custom_tags_resolve_unqualified_and_qualified() {
        let text = "Color :: [Red, Green]\n\nx = Red\ny = Color.Green\n";
        let analysis = analyze_text(text);
        let red = analysis
            .references
            .iter()
            .find(|reference| reference.text == "Red")
            .expect("Red reference");
        let green = analysis
            .references
            .iter()
            .find(|reference| reference.text == "Green")
            .expect("Green reference");
        assert!(red.key.is_some(), "{:?}", analysis.diagnostics);
        assert!(green.key.is_some(), "{:?}", analysis.diagnostics);
        assert!(!analysis
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("not exported")));
    }

    #[test]
    fn interpolations_are_parsed_for_references() {
        let text = "x = 1\nmsg = \"value ${x}\"\nmulti = \\\\line ${x}\n";
        let analysis = analyze_text(text);
        let refs = analysis
            .references
            .iter()
            .filter(|reference| reference.text == "x")
            .count();
        assert!(
            refs >= 2,
            "expected interpolation refs, got {refs}: {:?}",
            analysis.references
        );
    }

    #[test]
    fn qualified_tag_patterns_resolve_to_the_declaration() {
        let text = "Color :: [Red, Green]\n\nf = |c| match c {\n\tColor.Red => 1\n\t_ => 0\n}\n";
        let analysis = analyze_text(text);
        assert!(
            analysis.diagnostics.is_empty(),
            "{:?}",
            analysis.diagnostics
        );
        let declaration_start = text.find("Red").expect("declaration") as u32;
        let pattern_start = text.rfind("Red").expect("pattern") as u32;
        let key = DefinitionKey::Local {
            uri: Url::parse("file:///tmp/main.roc").unwrap(),
            def_start: declaration_start,
        };
        let pattern = analysis
            .references
            .iter()
            .find(|reference| reference.range.start == pattern_start)
            .expect("pattern reference");
        assert_eq!(pattern.key, Some(key));
    }

    #[test]
    fn record_builder_suffixes_resolve_the_nominal_type() {
        let text = "Builder :: { count : U64 }\n\nf = { count: 1 }.Builder\n";
        let analysis = analyze_text(text);
        assert!(
            analysis.diagnostics.is_empty(),
            "{:?}",
            analysis.diagnostics
        );
        let declaration_start = text.find("Builder").expect("declaration") as u32;
        let use_start = text.rfind("Builder").expect("use") as u32;
        let key = DefinitionKey::TopLevel {
            uri: Url::parse("file:///tmp/main.roc").unwrap(),
            name: "Builder".to_string(),
            container: None,
        };
        let usage = analysis
            .references
            .iter()
            .find(|reference| reference.range.start == use_start)
            .expect("builder reference");
        assert_eq!(usage.key, Some(key.clone()));
        assert!(declaration_start < use_start);
    }

    #[test]
    fn compiler_owned_glue_platforms_are_known_and_unindexed() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let root = temp.path();
        let text = "app [main!] { pf: platform glue, roc: \"nightly\" }\n\nimport pf.Types\n\nmain! = |_| {}\n";
        let path = root.join("main.roc");
        std::fs::write(&path, text).expect("main");
        let workspace = WorkspaceIndex::build(root, &AnalysisConfig::default());
        let package = workspace.package("pf").expect("glue platform alias");
        assert!(package.root.is_none());
        assert!(matches!(
            package.source,
            crate::syntax::ast::DependencySource::CompilerOwned(_)
        ));
        let uri = Url::from_file_path(&path).expect("uri");
        let parsed = parse(text);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let source = Arc::new(SourceFile::new(uri.clone(), text.to_string()));
        let analysis = analyze(
            uri,
            source,
            Arc::new(parsed),
            &workspace,
            &BuiltinIndex::empty(),
            &AnalysisConfig::default(),
        );
        assert!(
            !analysis
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "unresolved-import"),
            "{:?}",
            analysis.diagnostics
        );
    }

    #[test]
    fn workspace_root_relative_import_resolves_from_the_root() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let root = temp.path();
        std::fs::create_dir_all(root.join("Internal")).expect("dir");
        std::fs::write(
            root.join("Internal/Parser.roc"),
            "module [parse]\n\nparse = |n| n\n",
        )
        .expect("parser module");
        let text = "app [main!] { roc: \"nightly\" }\n\nimport /Internal/Parser\n\nmain! = |_| Parser.parse(1)\n";
        std::fs::write(root.join("main.roc"), text).expect("main");
        let path = root.join("main.roc");
        let workspace = WorkspaceIndex::build(root, &AnalysisConfig::default());
        let uri = Url::from_file_path(&path).expect("uri");
        let parsed = parse(text);
        let source = Arc::new(SourceFile::new(uri.clone(), text.to_string()));
        let analysis = analyze(
            uri,
            source,
            Arc::new(parsed),
            &workspace,
            &BuiltinIndex::empty(),
            &AnalysisConfig::default(),
        );
        assert_eq!(
            analysis.imports[0]
                .target
                .as_ref()
                .map(|target| target.name.as_str()),
            Some("Internal/Parser")
        );
        let reference = analysis
            .references
            .iter()
            .find(|reference| reference.text == "parse")
            .expect("parse reference");
        assert!(reference.key.is_some(), "{:?}", analysis.diagnostics);
    }

    #[test]
    fn missing_package_alias_does_not_fall_back_to_a_same_named_root_module() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let root = temp.path();
        std::fs::write(root.join("Ghost.roc"), "module [x]\n\nx = 1\n").expect("ghost module");
        let text = "app [main!] {\n\tghost: \"./missing/main.roc\",\n\troc: \"nightly\",\n}\n\nimport ghost.Ghost\n\nmain! = |_| Ghost.x\n";
        std::fs::write(root.join("main.roc"), text).expect("main");
        let path = root.join("main.roc");
        let workspace = WorkspaceIndex::build(root, &AnalysisConfig::default());
        let uri = Url::from_file_path(&path).expect("uri");
        let parsed = parse(text);
        let source = Arc::new(SourceFile::new(uri.clone(), text.to_string()));
        let analysis = analyze(
            uri,
            source,
            Arc::new(parsed),
            &workspace,
            &BuiltinIndex::empty(),
            &AnalysisConfig::default(),
        );
        assert!(analysis.imports[0].target.is_none());
        assert!(
            analysis
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "unresolved-import"),
            "{:?}",
            analysis.diagnostics
        );
    }

    #[test]
    fn annotation_and_definition_are_not_a_duplicate_definition() {
        let analysis = analyze_text("value : Str\nvalue = \"ok\"\n");
        assert!(
            !analysis
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "duplicate-definition"),
            "{:?}",
            analysis.diagnostics
        );
    }

    #[test]
    fn record_type_field_declarations_have_definition_keys() {
        let text = "Person :: { name : Str, age : U64 }\n\nperson = { name: \"Ada\", age: 36 }\n";
        let analysis = analyze_text(text);
        let fields: Vec<_> = analysis
            .references
            .iter()
            .filter(|reference| reference.text == "name" && reference.key.is_some())
            .collect();
        assert!(
            fields.len() >= 2,
            "declaration and record literal must both resolve"
        );
        assert_eq!(fields[0].key, fields[1].key);
        assert_eq!(fields[0].kind, SymbolKind::Field);
    }

    #[test]
    fn later_binding_in_the_same_scope_shadows_the_earlier_one() {
        let text = "f = |a| {\n\tx = a\n\tx = 2\n\tx\n}\n";
        let analysis = analyze_text(text);
        let tail = analysis
            .references
            .iter()
            .filter(|reference| reference.text == "x")
            .max_by_key(|reference| reference.range.start)
            .expect("tail x");
        let second_decl = text.find("\tx = 2").unwrap() as u32 + 1;
        assert_eq!(
            tail.key,
            Some(DefinitionKey::Local {
                uri: analysis.uri.clone(),
                def_start: second_decl,
            })
        );
    }
}

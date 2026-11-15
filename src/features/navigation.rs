//! Navigation: go-to, references, rename, and document highlights.
//!
//! All five requests share one resolver: an identifier occurrence is turned
//! into a stable [`DefinitionKey`] (by analysis, or by inspecting an import
//! path or qualified name chain), and the key is then resolved to workspace
//! locations.  This keeps cross-file behaviour consistent and makes read-only
//! boundaries (builtins, the compiler package cache) a property of the key
//! rather than of each request.

use std::collections::{HashMap, HashSet};

use lsp_types::{
    DocumentChanges, DocumentHighlight, DocumentHighlightKind, DocumentHighlightParams,
    GotoDefinitionParams, GotoDefinitionResponse, Location, OneOf,
    OptionalVersionedTextDocumentIdentifier, Position, PrepareRenameResponse, Range,
    ReferenceParams, RenameParams, TextDocumentEdit, TextDocumentPositionParams, TextEdit, Url,
    WorkspaceEdit,
};

use crate::analysis::packages::package_cache_dir;
use crate::analysis::{
    identifier_at, AnalyzedFile, DefinitionKey, ModuleSummary, SymbolInfo, SymbolKind,
};
use crate::syntax::ast::{Import, ImportPath, TypeExpr, TypeKind};
use crate::syntax::token::{Keyword, TokenKind};
use crate::text::{path_to_uri, SourceFile, TextRange};

use super::FeatureContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationKind {
    Definition,
    Declaration,
    TypeDefinition,
    Implementation,
}

pub fn goto(
    ctx: &FeatureContext<'_>,
    params: &GotoDefinitionParams,
    kind: NavigationKind,
) -> Option<GotoDefinitionResponse> {
    let offset = offset_at(ctx, params.text_document_position_params.position);
    let locations = match kind {
        NavigationKind::Definition | NavigationKind::Declaration => {
            if let Some(location) = import_or_dependency_location(ctx, offset) {
                vec![location]
            } else if let Some(key) = key_at(ctx, offset) {
                definition_locations(ctx, &key)
            } else {
                Vec::new()
            }
        }
        NavigationKind::TypeDefinition => match key_at(ctx, offset) {
            Some(key) => type_definition_locations(ctx, &key),
            None => Vec::new(),
        },
        NavigationKind::Implementation => match key_at(ctx, offset) {
            Some(key) => implementation_locations(ctx, &key),
            None => Vec::new(),
        },
    };
    let locations = dedupe_locations(locations);
    if locations.is_empty() {
        None
    } else {
        Some(GotoDefinitionResponse::Array(locations))
    }
}

pub fn references(ctx: &FeatureContext<'_>, params: &ReferenceParams) -> Vec<Location> {
    let offset = offset_at(ctx, params.text_document_position.position);
    let Some(key) = key_at(ctx, offset) else {
        return Vec::new();
    };
    collect_references(ctx, &key, params.context.include_declaration)
}

pub fn prepare_rename(
    ctx: &FeatureContext<'_>,
    params: &TextDocumentPositionParams,
) -> Option<PrepareRenameResponse> {
    let offset = offset_at(ctx, params.position);
    let key = key_at(ctx, offset)?;
    if !key_is_writable(ctx, &key) {
        return None;
    }
    let (placeholder, range) = cursor_name(ctx, offset)?;
    Some(PrepareRenameResponse::RangeWithPlaceholder {
        range: ctx.analysis.source.range(range),
        placeholder,
    })
}

pub fn rename(ctx: &FeatureContext<'_>, params: &RenameParams) -> Option<WorkspaceEdit> {
    let offset = offset_at(ctx, params.text_document_position.position);
    let key = key_at(ctx, offset)?;
    if !key_is_writable(ctx, &key) {
        return None;
    }
    let old_name = key_name(ctx, &key)?;
    if !valid_new_name(&old_name, &params.new_name) {
        return None;
    }
    if has_collision(ctx, &key, &params.new_name) {
        return None;
    }

    let mut by_uri: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    for location in collect_references(ctx, &key, true) {
        if !uri_is_writable(ctx, &location.uri) {
            continue;
        }
        let edits = by_uri.entry(location.uri).or_default();
        let edit = TextEdit {
            range: location.range,
            new_text: params.new_name.clone(),
        };
        if !edits
            .iter()
            .any(|existing| existing.range == edit.range && existing.new_text == edit.new_text)
        {
            edits.push(edit);
        }
    }
    if by_uri.is_empty() {
        return None;
    }

    let mut documents: Vec<TextDocumentEdit> = by_uri
        .into_iter()
        .map(|(uri, mut edits)| {
            edits.sort_by(|a, b| {
                (a.range.start.line, a.range.start.character)
                    .cmp(&(b.range.start.line, b.range.start.character))
            });
            TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
                edits: edits.into_iter().map(OneOf::Left).collect(),
            }
        })
        .collect();
    documents.sort_by(|a, b| {
        a.text_document
            .uri
            .as_str()
            .cmp(b.text_document.uri.as_str())
    });

    Some(WorkspaceEdit {
        changes: None,
        document_changes: Some(DocumentChanges::Edits(documents)),
        change_annotations: None,
    })
}

pub fn highlights(
    ctx: &FeatureContext<'_>,
    params: &DocumentHighlightParams,
) -> Vec<DocumentHighlight> {
    let offset = offset_at(ctx, params.text_document_position_params.position);
    if let Some(key) = key_at(ctx, offset) {
        let writes = write_ranges(ctx, &key);
        let mut ranges = current_file_ranges(ctx, &key);
        if matches!(&key, DefinitionKey::Builtin { .. }) {
            let short = key_name(ctx, &key).unwrap_or_default();
            scan_word_tokens(ctx.analysis, &short, &mut ranges);
        }
        let mut highlights: Vec<DocumentHighlight> = ranges
            .into_iter()
            .map(|range| DocumentHighlight {
                range: ctx.analysis.source.range(range),
                kind: Some(if writes.contains(&range) {
                    DocumentHighlightKind::WRITE
                } else {
                    DocumentHighlightKind::READ
                }),
            })
            .collect();
        dedupe_highlights(&mut highlights);
        if !highlights.is_empty() {
            return highlights;
        }
    }

    // With no resolved key, be deliberately conservative: only highlight the
    // exact word when it names a binding visible at the cursor.
    let Some((word, _)) = word_at(ctx, offset) else {
        return Vec::new();
    };
    let Some(binding) = ctx
        .analysis
        .visible_bindings(offset)
        .into_iter()
        .find(|binding| binding.name == word)
    else {
        return Vec::new();
    };
    let declaration = binding.name_range;
    let mut ranges = Vec::new();
    scan_word_tokens(ctx.analysis, &word, &mut ranges);
    let mut highlights: Vec<DocumentHighlight> = ranges
        .into_iter()
        .map(|range| DocumentHighlight {
            range: ctx.analysis.source.range(range),
            kind: Some(if range == declaration {
                DocumentHighlightKind::WRITE
            } else {
                DocumentHighlightKind::READ
            }),
        })
        .collect();
    dedupe_highlights(&mut highlights);
    highlights
}

// ---------------------------------------------------------------------------
// Identifier and key resolution
// ---------------------------------------------------------------------------

fn offset_at(ctx: &FeatureContext<'_>, position: Position) -> u32 {
    ctx.analysis.source.offset_at(position)
}

fn word_at(ctx: &FeatureContext<'_>, offset: u32) -> Option<(String, TextRange)> {
    ctx.analysis
        .word_at(offset)
        .or_else(|| identifier_at(&ctx.analysis.source, offset))
}

fn cursor_name(ctx: &FeatureContext<'_>, offset: u32) -> Option<(String, TextRange)> {
    if let Some(reference) = ctx.analysis.reference_at(offset) {
        return Some((reference.text.clone(), reference.range));
    }
    if let Some(symbol) = ctx.analysis.symbol_at(offset) {
        return Some((symbol.name.clone(), symbol.name_range));
    }
    word_at(ctx, offset)
}

fn key_at(ctx: &FeatureContext<'_>, offset: u32) -> Option<DefinitionKey> {
    if let Some(key) = import_module_key(ctx, offset) {
        return Some(key);
    }
    if let Some(key) = ctx.analysis.definition_key_at(offset) {
        return Some(key.clone());
    }
    if let Some(key) = chain_key(ctx, ctx.analysis, offset) {
        return Some(key);
    }
    let (word, _) = word_at(ctx, offset)?;
    if word
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
    {
        if let Some(key) = resolve_simple_name(ctx, ctx.analysis, &word) {
            return Some(key);
        }
    }
    ctx.analysis
        .visible_bindings(offset)
        .into_iter()
        .find(|binding| binding.name == word)
        .map(|binding| binding.key.clone())
}

fn key_name(ctx: &FeatureContext<'_>, key: &DefinitionKey) -> Option<String> {
    match key {
        DefinitionKey::Local { uri, def_start } => with_analysis(ctx, uri, |analysis| {
            declaration_name_at(analysis, *def_start)
        })
        .flatten(),
        DefinitionKey::TopLevel { name, .. } | DefinitionKey::Builtin { full_name: name } => {
            Some(name.clone())
        }
        DefinitionKey::Module { uri } => ctx
            .host
            .workspace()
            .module_for_uri(uri)
            .map(|module| module.name.clone())
            .or_else(|| {
                uri.to_file_path().ok().and_then(|path| {
                    path.file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                })
            }),
    }
}

// ---------------------------------------------------------------------------
// Qualified chains (`Color.to_str`, `Str.concat`, `Url.ParseErr`)
// ---------------------------------------------------------------------------

fn is_ident_token(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::LowerIdent | TokenKind::UpperIdent)
}

fn token_index_at(analysis: &AnalyzedFile, offset: u32) -> Option<usize> {
    analysis
        .parsed
        .tokens
        .iter()
        .position(|token| token.range.contains_inclusive(offset) && is_ident_token(token.kind))
}

fn prev_code(tokens: &[crate::syntax::token::Token], index: usize) -> Option<usize> {
    (0..index).rev().find(|&i| !tokens[i].kind.is_trivia())
}

fn next_code(tokens: &[crate::syntax::token::Token], index: usize) -> Option<usize> {
    (index + 1..tokens.len()).find(|&i| !tokens[i].kind.is_trivia())
}

fn chain_at(analysis: &AnalyzedFile, offset: u32) -> Option<(Vec<(String, TextRange)>, usize)> {
    let tokens = &analysis.parsed.tokens;
    let index = token_index_at(analysis, offset)?;
    let text = tokens[index].text(analysis.source.text()).to_string();
    let mut chain = vec![(text, tokens[index].range)];
    let mut cursor = 0;

    let mut current = index;
    while let Some(dot) = prev_code(tokens, current) {
        if tokens[dot].kind != TokenKind::Dot {
            break;
        }
        let Some(ident) = prev_code(tokens, dot) else {
            break;
        };
        if !is_ident_token(tokens[ident].kind) {
            break;
        }
        chain.insert(
            0,
            (
                tokens[ident].text(analysis.source.text()).to_string(),
                tokens[ident].range,
            ),
        );
        cursor += 1;
        current = ident;
    }

    let mut current = index;
    while let Some(dot) = next_code(tokens, current) {
        if tokens[dot].kind != TokenKind::Dot {
            break;
        }
        let Some(ident) = next_code(tokens, dot) else {
            break;
        };
        if !is_ident_token(tokens[ident].kind) {
            break;
        }
        chain.push((
            tokens[ident].text(analysis.source.text()).to_string(),
            tokens[ident].range,
        ));
        current = ident;
    }

    Some((chain, cursor))
}

fn import_module_key(ctx: &FeatureContext<'_>, offset: u32) -> Option<DefinitionKey> {
    let import = ctx
        .analysis
        .module()
        .imports
        .iter()
        .find(|import| import.range.contains_inclusive(offset))?;
    let names_module = import
        .alias
        .as_ref()
        .is_some_and(|alias| alias.range.contains_inclusive(offset))
        || import.path.range().contains_inclusive(offset);
    if !names_module {
        return None;
    }
    let target = resolve_import_target(ctx, ctx.analysis, import)?;
    Some(DefinitionKey::Module { uri: target.uri })
}

fn chain_key(
    ctx: &FeatureContext<'_>,
    analysis: &AnalyzedFile,
    offset: u32,
) -> Option<DefinitionKey> {
    let (chain, cursor) = chain_at(analysis, offset)?;
    resolve_chain(ctx, analysis, &chain, cursor)
}

fn resolve_chain(
    ctx: &FeatureContext<'_>,
    analysis: &AnalyzedFile,
    chain: &[(String, TextRange)],
    cursor: usize,
) -> Option<DefinitionKey> {
    if chain.len() < 2 {
        return None;
    }
    let first = &chain[0].0;
    let last = &chain[chain.len() - 1].0;

    // An imported module or alias introduces the qualifier.
    if let Some(resolved) = analysis
        .imports
        .iter()
        .find(|resolved| resolved.qualifier == *first)
    {
        if let Some(target) = resolve_import_target(ctx, analysis, &resolved.import) {
            if cursor == 0 {
                return Some(DefinitionKey::Module { uri: target.uri });
            }
            let container = if chain.len() > 2 {
                Some(chain[1].0.as_str())
            } else {
                None
            };
            return export_key(target.summary.as_ref(), &target.uri, last, container);
        }
    }

    // A nominal type declared in this file: `Color.Red`, `Point.{...}`.
    if let Some(symbol) = find_symbol(&analysis.symbols, first, None) {
        if cursor == 0 {
            return Some(symbol.key.clone());
        }
        if matches!(symbol.kind, SymbolKind::Type) {
            if let Some(member) = find_symbol(&symbol.children, last, Some(symbol.name.as_str())) {
                return Some(member.key.clone());
            }
            return Some(DefinitionKey::TopLevel {
                uri: analysis.uri.clone(),
                name: last.clone(),
                container: Some(symbol.name.clone()),
            });
        }
    }

    // Builtins: `Str.concat`, or the `Str` module itself.
    let full = chain
        .iter()
        .map(|(text, _)| text.as_str())
        .collect::<Vec<_>>()
        .join(".");
    if let Some(item) = ctx.host.builtins().lookup(&full) {
        return Some(item.key());
    }
    if let Some(module) = ctx.host.builtins().module(first) {
        if cursor == 0 {
            return Some(DefinitionKey::Builtin {
                full_name: first.clone(),
            });
        }
        if let Some(item) = module.items.iter().find(|item| item.short_name == *last) {
            return Some(item.key());
        }
    }

    // A workspace module used without an explicit import (platform modules).
    let workspace = ctx.host.workspace();
    if let Some(module) = workspace
        .modules
        .iter()
        .find(|module| module.name == *first)
    {
        let module = module.clone();
        if cursor == 0 {
            return Some(DefinitionKey::Module {
                uri: module.uri.clone(),
            });
        }
        let container = if chain.len() > 2 {
            Some(chain[1].0.as_str())
        } else {
            None
        };
        return export_key(Some(&module), &module.uri.clone(), last, container);
    }

    None
}

fn export_key(
    summary: Option<&ModuleSummary>,
    uri: &Url,
    name: &str,
    container: Option<&str>,
) -> Option<DefinitionKey> {
    if let Some(summary) = summary {
        let export = find_export(summary, name, container)?;
        if let Some(key) = &export.key {
            return Some(key.clone());
        }
        return Some(DefinitionKey::TopLevel {
            uri: summary.uri.clone(),
            name: name.to_string(),
            container: export
                .container
                .clone()
                .or_else(|| container.map(str::to_string)),
        });
    }
    Some(DefinitionKey::TopLevel {
        uri: uri.clone(),
        name: name.to_string(),
        container: container.map(str::to_string),
    })
}

fn find_export<'a>(
    module: &'a ModuleSummary,
    name: &str,
    container: Option<&str>,
) -> Option<&'a crate::analysis::index::Export> {
    let exports = module.all_exports();
    if let Some(container) = container {
        return exports
            .into_iter()
            .find(|export| export.name == name && export.container.as_deref() == Some(container));
    }
    if let Some(export) = exports
        .iter()
        .find(|export| export.name == name && export.container.is_none())
        .copied()
    {
        return Some(export);
    }
    exports.into_iter().find(|export| export.name == name)
}

// ---------------------------------------------------------------------------
// Import and dependency targets
// ---------------------------------------------------------------------------

struct TargetModule {
    uri: Url,
    summary: Option<ModuleSummary>,
}

fn import_or_dependency_location(ctx: &FeatureContext<'_>, offset: u32) -> Option<Location> {
    if let Some(location) = import_location(ctx, offset) {
        return Some(location);
    }
    dependency_location(ctx, offset)
}

fn import_location(ctx: &FeatureContext<'_>, offset: u32) -> Option<Location> {
    let import = ctx
        .analysis
        .module()
        .imports
        .iter()
        .find(|import| import.range.contains_inclusive(offset))?;

    // `import Color as C`: the alias names the module itself.
    if let Some(alias) = &import.alias {
        if alias.range.contains_inclusive(offset) {
            let target = resolve_import_target(ctx, ctx.analysis, import)?;
            return Some(location_for(ctx, &target.uri, TextRange::empty(0)));
        }
    }

    // `exposing [to_str]`: resolve the exposed item in the target module.
    for name in &import.exposing {
        if name.range.contains_inclusive(offset) {
            let target = resolve_import_target(ctx, ctx.analysis, import)?;
            if let Some(summary) = &target.summary {
                return export_location(ctx, summary, &name.text, None);
            }
            return Some(location_for(ctx, &target.uri, TextRange::empty(0)));
        }
    }

    match &import.path {
        ImportPath::File { range, .. } => {
            if !range.contains_inclusive(offset) {
                return None;
            }
            let target = resolve_import_target(ctx, ctx.analysis, import)?;
            Some(location_for(ctx, &target.uri, TextRange::empty(0)))
        }
        ImportPath::Roc {
            segments, range, ..
        } => {
            if !range.contains_inclusive(offset) {
                return None;
            }
            let segment_index = segments
                .iter()
                .position(|segment| segment.range.contains_inclusive(offset));
            let names: Vec<String> = segments
                .iter()
                .map(|segment| segment.text.clone())
                .collect();

            // A nested type (`Url.ParseErr`, `cli.Url.ParseErr`): try the
            // longest prefix that resolves to a module, then its last segment
            // as an associated type.
            if names.len() > 1 && segment_index.is_some_and(|index| index == names.len() - 1) {
                if let Some(location) = nested_import_location(ctx, ctx.analysis, &names) {
                    return Some(location);
                }
            }

            // Cursor on the first segment of a multi-part path: prefer the
            // module it qualifies.
            if names.len() > 1 && segment_index == Some(0) {
                if let Some(module) = find_module_by_name(ctx, &names[0]) {
                    return Some(location_for(ctx, &module.uri, TextRange::empty(0)));
                }
            }

            let target = resolve_import_target(ctx, ctx.analysis, import)?;
            Some(location_for(ctx, &target.uri, TextRange::empty(0)))
        }
    }
}

fn nested_import_location(
    ctx: &FeatureContext<'_>,
    analysis: &AnalyzedFile,
    names: &[String],
) -> Option<Location> {
    // Longest prefix that is a real module (package + module, or module alone).
    for split in (1..names.len()).rev() {
        let raw = names[..split].join(".");
        let Some(module) = find_module_for_raw(ctx, &raw, &names[..split], analysis) else {
            continue;
        };
        let last = &names[names.len() - 1];
        let container = if names.len() - split > 1 {
            Some(names[split].as_str())
        } else {
            None
        };
        if let Some(location) = export_location(ctx, &module, last, container) {
            return Some(location);
        }
    }
    None
}

fn export_location(
    ctx: &FeatureContext<'_>,
    module: &ModuleSummary,
    name: &str,
    container: Option<&str>,
) -> Option<Location> {
    let export = find_export(module, name, container)?;
    if let Some(key) = &export.key {
        if let Some(location) = resolve_location(ctx, key) {
            return Some(location);
        }
    }
    Some(location_for(ctx, &module.uri, export.selection_range))
}

fn resolve_import_target(
    ctx: &FeatureContext<'_>,
    analysis: &AnalyzedFile,
    import: &Import,
) -> Option<TargetModule> {
    if let Some(resolved) = analysis
        .imports
        .iter()
        .find(|resolved| resolved.import.range == import.range)
    {
        if let Some(target) = &resolved.target {
            return Some(TargetModule {
                uri: target.uri.clone(),
                summary: Some(target.clone()),
            });
        }
        if let Some(uri) = &resolved.file_uri {
            let summary = ctx.host.workspace().module_for_uri(uri).cloned();
            return Some(TargetModule {
                uri: uri.clone(),
                summary,
            });
        }
    }

    let workspace = ctx.host.workspace();
    match &import.path {
        ImportPath::File { path, .. } => {
            let base = analysis.uri.to_file_path().ok()?.parent()?.to_path_buf();
            let full = crate::analysis::packages::normalize_path(&base.join(path));
            let uri = path_to_uri(&full)?;
            let summary = workspace.module_for_path(&full).cloned();
            Some(TargetModule { uri, summary })
        }
        ImportPath::Roc { raw, segments, .. } => {
            let names: Vec<String> = segments
                .iter()
                .map(|segment| segment.text.clone())
                .collect();
            if let Some(module) = find_module_for_raw(ctx, raw, &names, analysis) {
                return Some(TargetModule {
                    uri: module.uri.clone(),
                    summary: Some(module),
                });
            }

            // A bare package shorthand resolves to its root entry point.
            if let Some(package) = workspace.package(raw) {
                if let Some(root_file) = &package.root_file {
                    return Some(TargetModule {
                        uri: path_to_uri(root_file)?,
                        summary: None,
                    });
                }
                if let Some(root) = &package.root {
                    for entry in ["main.roc", "app.roc", "package.roc"] {
                        let candidate = root.join(entry);
                        if candidate.is_file() {
                            return Some(TargetModule {
                                uri: path_to_uri(&candidate)?,
                                summary: None,
                            });
                        }
                    }
                }
            }

            // A local module path relative to the importing file, or to the
            // workspace root when it starts with `/`.
            let root_relative = raw.starts_with('/');
            let base = if root_relative {
                workspace.root.clone()
            } else {
                analysis.uri.to_file_path().ok()?.parent()?.to_path_buf()
            };
            let trimmed = raw.trim_start_matches('/').trim_end_matches(".roc");
            for candidate in [format!("{trimmed}.roc"), format!("{trimmed}/main.roc")] {
                let full = crate::analysis::packages::normalize_path(&base.join(candidate));
                if let Some(module) = workspace.module_for_path(&full) {
                    return Some(TargetModule {
                        uri: module.uri.clone(),
                        summary: Some(module.clone()),
                    });
                }
                if full.is_file() {
                    return Some(TargetModule {
                        uri: path_to_uri(&full)?,
                        summary: None,
                    });
                }
            }
            None
        }
    }
}

fn find_module_by_name(ctx: &FeatureContext<'_>, name: &str) -> Option<ModuleSummary> {
    ctx.host
        .workspace()
        .modules
        .iter()
        .find(|module| module.name == name)
        .cloned()
}

fn find_module_for_raw(
    ctx: &FeatureContext<'_>,
    raw: &str,
    names: &[String],
    analysis: &AnalyzedFile,
) -> Option<ModuleSummary> {
    let workspace = ctx.host.workspace();
    let trimmed = raw.trim_end_matches(".roc");
    let dotted = trimmed.replace('/', ".");

    for module in &workspace.modules {
        if module.name == trimmed || module.name == dotted {
            return Some(module.clone());
        }
    }

    // `package.Module` (possibly with a nested type suffix).
    if names.len() > 1 && workspace.package(&names[0]).is_some() {
        let package = &names[0];
        let rest = names[1..].join(".");
        if let Some(module) = workspace.find_module(Some(package), &rest) {
            return Some(module.clone());
        }
        return None;
    }

    // Relative filesystem path.
    if let Ok(path) = analysis.uri.to_file_path() {
        if let Some(directory) = path.parent() {
            for candidate in [format!("{trimmed}.roc"), trimmed.to_string()] {
                let full = crate::analysis::packages::normalize_path(&directory.join(candidate));
                if let Some(module) = workspace.module_for_path(&full) {
                    return Some(module.clone());
                }
            }
        }
    }

    // Last resort: the first segment names the module and the rest is a
    // nested type (`Url.ParseErr`).
    if names.len() > 1 {
        if let Some(module) = workspace
            .modules
            .iter()
            .find(|module| module.name == names[0])
        {
            return Some(module.clone());
        }
    }
    if let Some(last) = names.last() {
        if let Some(module) = workspace.modules.iter().find(|module| module.name == *last) {
            return Some(module.clone());
        }
    }
    None
}

fn dependency_location(ctx: &FeatureContext<'_>, offset: u32) -> Option<Location> {
    let header = ctx.analysis.module().header.as_ref()?;
    let dependency = header.dependencies.iter().find(|dependency| {
        dependency.shorthand.range.contains_inclusive(offset)
            || dependency.source_range.contains_inclusive(offset)
    })?;
    let workspace = ctx.host.workspace();
    let package = workspace.package(&dependency.shorthand.text)?;
    let entry = package
        .root_file
        .clone()
        .filter(|path| path.is_file())
        .or_else(|| {
            let root = package.root.as_ref()?;
            ["main.roc", "app.roc", "package.roc"]
                .iter()
                .map(|name| root.join(name))
                .find(|path| path.is_file())
        })?;
    Some(location_for(
        ctx,
        &path_to_uri(&entry)?,
        TextRange::empty(0),
    ))
}

// ---------------------------------------------------------------------------
// Key -> location
// ---------------------------------------------------------------------------

fn with_analysis<R>(
    ctx: &FeatureContext<'_>,
    uri: &Url,
    f: impl FnOnce(&AnalyzedFile) -> R,
) -> Option<R> {
    if uri == &ctx.analysis.uri {
        return Some(f(ctx.analysis));
    }
    ctx.host.analyze(uri).map(|analysis| f(&analysis))
}

fn key_uri(key: &DefinitionKey) -> Option<&Url> {
    match key {
        DefinitionKey::Local { uri, .. }
        | DefinitionKey::TopLevel { uri, .. }
        | DefinitionKey::Module { uri } => Some(uri),
        DefinitionKey::Builtin { .. } => None,
    }
}

fn resolve_location(ctx: &FeatureContext<'_>, key: &DefinitionKey) -> Option<Location> {
    match key {
        DefinitionKey::Builtin { .. } => None,
        DefinitionKey::Module { uri } => Some(location_for(ctx, uri, TextRange::empty(0))),
        DefinitionKey::Local { uri, def_start } => {
            let range = with_analysis(ctx, uri, |analysis| {
                declaration_range_at(analysis, *def_start)
            })
            .flatten()
            .unwrap_or(TextRange::empty(*def_start));
            Some(location_for(ctx, uri, range))
        }
        DefinitionKey::TopLevel {
            uri,
            name,
            container,
        } => {
            let found = with_analysis(ctx, uri, |analysis| {
                find_symbol(&analysis.symbols, name, container.as_deref())
                    .map(|symbol| symbol.name_range)
            })
            .flatten();
            if let Some(range) = found {
                return Some(location_for(ctx, uri, range));
            }
            let range = ctx
                .host
                .workspace()
                .module_for_uri(uri)
                .and_then(|module| module.export_for_key(key))
                .map(|export| export.selection_range);
            if let Some(range) = range {
                return Some(location_for(ctx, uri, range));
            }
            ctx.host
                .find_definition(key)
                .map(|(uri, range)| location_for(ctx, &uri, range))
        }
    }
}

fn location_for(ctx: &FeatureContext<'_>, uri: &Url, range: TextRange) -> Location {
    let range = range_value_for(ctx, uri, range);
    Location {
        uri: uri.clone(),
        range,
    }
}

fn range_value_for(ctx: &FeatureContext<'_>, uri: &Url, range: TextRange) -> Range {
    if uri == &ctx.analysis.uri {
        return ctx.analysis.source.range(range);
    }
    if let Some(analysis) = ctx.host.analyze(uri) {
        return analysis.source.range(range);
    }
    if let Ok(path) = uri.to_file_path() {
        if let Ok(text) = std::fs::read_to_string(path) {
            let source = SourceFile::new(uri.clone(), text);
            return source.range(range);
        }
    }
    Range {
        start: Position::new(0, 0),
        end: Position::new(0, 0),
    }
}

fn definition_locations(ctx: &FeatureContext<'_>, key: &DefinitionKey) -> Vec<Location> {
    resolve_location(ctx, key).into_iter().collect()
}

fn type_definition_locations(ctx: &FeatureContext<'_>, key: &DefinitionKey) -> Vec<Location> {
    match key {
        DefinitionKey::TopLevel {
            uri,
            name,
            container,
        } => {
            let is_type = with_analysis(ctx, uri, |analysis| {
                find_symbol(&analysis.symbols, name, container.as_deref())
                    .is_some_and(|symbol| matches!(symbol.kind, SymbolKind::Type))
            })
            .unwrap_or(false);
            if !is_type {
                if let Some(type_key) = type_annotation_key(ctx, uri, name) {
                    if let Some(location) = resolve_location(ctx, &type_key) {
                        return vec![location];
                    }
                }
            }
            definition_locations(ctx, key)
        }
        _ => definition_locations(ctx, key),
    }
}

fn type_annotation_key(
    ctx: &FeatureContext<'_>,
    uri: &Url,
    value_name: &str,
) -> Option<DefinitionKey> {
    with_analysis(ctx, uri, |analysis| {
        let definition = analysis.definitions().into_iter().find(|definition| {
            definition
                .simple_name()
                .is_some_and(|name| name.text == value_name)
        })?;
        let annotation = definition.annotation.as_ref()?;
        resolve_type_ref(ctx, analysis, annotation)
    })
    .flatten()
}

fn resolve_type_ref(
    ctx: &FeatureContext<'_>,
    analysis: &AnalyzedFile,
    ty: &TypeExpr,
) -> Option<DefinitionKey> {
    match &ty.kind {
        TypeKind::Named(name) | TypeKind::Apply { name, .. } => {
            resolve_type_name(ctx, analysis, name)
        }
        TypeKind::Function { ret, .. } => resolve_type_ref(ctx, analysis, ret),
        TypeKind::Where { base, .. } => resolve_type_ref(ctx, analysis, base),
        _ => None,
    }
}

fn resolve_simple_name(
    ctx: &FeatureContext<'_>,
    analysis: &AnalyzedFile,
    text: &str,
) -> Option<DefinitionKey> {
    if let Some(symbol) = find_symbol(&analysis.symbols, text, None) {
        return Some(symbol.key.clone());
    }
    if let Some(resolved) = analysis.imports.iter().find(|resolved| {
        resolved.qualifier == text || resolved.exposing.iter().any(|exposed| exposed == text)
    }) {
        if let Some(target) = resolve_import_target(ctx, analysis, &resolved.import) {
            if let Some(key) = export_key(target.summary.as_ref(), &target.uri, text, None) {
                return Some(key);
            }
            return Some(DefinitionKey::TopLevel {
                uri: target.uri,
                name: text.to_string(),
                container: None,
            });
        }
    }
    ctx.host.builtins().lookup(text).map(|item| item.key())
}

fn resolve_type_name(
    ctx: &FeatureContext<'_>,
    analysis: &AnalyzedFile,
    name: &crate::syntax::ast::QualName,
) -> Option<DefinitionKey> {
    if name.parts.len() == 1 {
        return resolve_simple_name(ctx, analysis, &name.parts[0].text);
    }

    let qualifier = &name.parts[0].text;
    let last = &name.parts[name.parts.len() - 1].text;
    if let Some(resolved) = analysis
        .imports
        .iter()
        .find(|resolved| resolved.qualifier == *qualifier)
    {
        if let Some(target) = resolve_import_target(ctx, analysis, &resolved.import) {
            let container = if name.parts.len() > 2 {
                Some(name.parts[1].text.as_str())
            } else {
                None
            };
            return export_key(target.summary.as_ref(), &target.uri, last, container);
        }
    }
    let full = name.as_string();
    ctx.host.builtins().lookup(&full).map(|item| item.key())
}

fn implementation_locations(ctx: &FeatureContext<'_>, key: &DefinitionKey) -> Vec<Location> {
    if let DefinitionKey::Module { uri } = key {
        if let Some(module) = ctx.host.workspace().module_for_uri(uri).cloned() {
            let mut locations = Vec::new();
            if let Some(export) = module
                .all_exports()
                .into_iter()
                .find(|export| export.name == module.name && !export.children.is_empty())
            {
                for child in &export.children {
                    if let Some(location) = export_child_location(ctx, &module, child) {
                        locations.push(location);
                    }
                }
            }
            if !locations.is_empty() {
                return dedupe_locations(locations);
            }
        }
    }
    if let DefinitionKey::TopLevel {
        uri,
        name,
        container: None,
    } = key
    {
        let mut locations = Vec::new();
        let symbol = with_analysis(ctx, uri, |analysis| {
            find_symbol(&analysis.symbols, name, None).cloned()
        })
        .flatten();
        if let Some(symbol) = symbol {
            if matches!(symbol.kind, SymbolKind::Type) {
                for child in &symbol.children {
                    if matches!(child.kind, SymbolKind::Method) {
                        if let Some(location) = resolve_location(ctx, &child.key) {
                            locations.push(location);
                        }
                    }
                }
            }
        }
        if locations.is_empty() {
            if let Some(module) = ctx.host.workspace().module_for_uri(uri).cloned() {
                if let Some(export) = module
                    .all_exports()
                    .into_iter()
                    .find(|export| export.name == *name && export.container.is_none())
                {
                    for child in &export.children {
                        if let Some(location) = export_child_location(ctx, &module, child) {
                            locations.push(location);
                        }
                    }
                }
            }
        }
        if locations.is_empty() {
            locations.extend(definition_locations(ctx, key));
        }
        return dedupe_locations(locations);
    }
    definition_locations(ctx, key)
}

fn export_child_location(
    ctx: &FeatureContext<'_>,
    module: &ModuleSummary,
    export: &crate::analysis::index::Export,
) -> Option<Location> {
    if let Some(key) = &export.key {
        if let Some(location) = resolve_location(ctx, key) {
            return Some(location);
        }
    }
    Some(location_for(ctx, &module.uri, export.selection_range))
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

fn collect_references(
    ctx: &FeatureContext<'_>,
    key: &DefinitionKey,
    include_declaration: bool,
) -> Vec<Location> {
    let declaration = resolve_location(ctx, key).map(|location| (location.uri, location.range));

    let mut locations = Vec::new();
    let mut seen = HashSet::new();

    let push = |uri: Url,
                range: Range,
                locations: &mut Vec<Location>,
                seen: &mut HashSet<(String, u32, u32, u32, u32)>| {
        let id = (
            uri.as_str().to_string(),
            range.start.line,
            range.start.character,
            range.end.line,
            range.end.character,
        );
        if seen.insert(id) {
            locations.push(Location { uri, range });
        }
    };

    // The current file first: the request may come from an unsaved buffer that
    // is not part of the workspace index.
    for (uri, range) in occurrences_in(ctx, ctx.analysis, key) {
        push(uri, range, &mut locations, &mut seen);
    }

    let mut analysed: HashSet<String> = HashSet::new();
    analysed.insert(ctx.analysis.uri.as_str().to_string());
    for analysis in related_analyses(ctx, key) {
        if !analysed.insert(analysis.uri.as_str().to_string()) {
            continue;
        }
        for (uri, range) in occurrences_in(ctx, &analysis, key) {
            push(uri, range, &mut locations, &mut seen);
        }
    }

    if include_declaration {
        if let Some((uri, range)) = declaration {
            // Module declarations are represented by their import sites, not by
            // an empty range at the start of the file.
            if !range_is_empty(&range) {
                push(uri, range, &mut locations, &mut seen);
            }
        }
    } else if let Some((declaration_uri, declaration_range)) = &declaration {
        locations.retain(|location| {
            !(&location.uri == declaration_uri && location.range == *declaration_range)
        });
    }

    sort_locations(&mut locations);
    locations
}

fn related_analyses(
    ctx: &FeatureContext<'_>,
    key: &DefinitionKey,
) -> Vec<std::sync::Arc<AnalyzedFile>> {
    let mut analyses = ctx.host.all_analyses();
    let Some(def_uri) = key_uri(key) else {
        return analyses;
    };

    let mut seen: HashSet<String> = analyses
        .iter()
        .map(|analysis| analysis.uri.as_str().to_string())
        .collect();
    if seen.insert(def_uri.as_str().to_string()) {
        if let Some(analysis) = ctx.host.analyze(def_uri) {
            analyses.push(analysis);
        }
    }

    // If the definition lives in a package, occurrences can also live in the
    // package's other modules.  Load them from the workspace summaries so
    // references and rename see package-internal uses without scanning the
    // entire compiler cache.
    let workspace = ctx.host.workspace();
    let package_root = workspace
        .module_for_uri(def_uri)
        .and_then(|module| module.package_root.clone())
        .or_else(|| {
            def_uri
                .to_file_path()
                .ok()
                .and_then(|path| workspace.package_for_path(&path))
                .and_then(|package| package.root.clone())
        });
    if let Some(package_root) = package_root {
        let modules: Vec<Url> = workspace
            .modules
            .iter()
            .filter(|module| module.package_root.as_deref() == Some(package_root.as_path()))
            .map(|module| module.uri.clone())
            .collect();
        for uri in modules {
            if seen.insert(uri.as_str().to_string()) {
                if let Some(analysis) = ctx.host.analyze(&uri) {
                    analyses.push(analysis);
                }
            }
        }
    }

    analyses
}

fn occurrences_in(
    ctx: &FeatureContext<'_>,
    analysis: &AnalyzedFile,
    key: &DefinitionKey,
) -> Vec<(Url, Range)> {
    let mut ranges: Vec<TextRange> = Vec::new();

    for reference in &analysis.references {
        if reference.key.as_ref() == Some(key) {
            ranges.push(reference.range);
        }
    }

    if let DefinitionKey::Module { uri } = key {
        for import in &analysis.module().imports {
            let path_matches = match &import.path {
                ImportPath::File { range, .. } => !range.is_empty(),
                ImportPath::Roc { range, .. } => !range.is_empty(),
            };
            if !path_matches {
                continue;
            }
            if let Some(target) = resolve_import_target(ctx, analysis, import) {
                if target.uri == *uri {
                    ranges.push(import.path.range());
                }
            }
        }
    }

    if let DefinitionKey::TopLevel {
        name,
        container: None,
        ..
    } = key
    {
        for import in &analysis.module().imports {
            for exposed in &import.exposing {
                if exposed.text != *name {
                    continue;
                }
                if let Some(target) = resolve_import_target(ctx, analysis, import) {
                    if Some(&target.uri) == key_uri(key) {
                        ranges.push(exposed.range);
                    }
                }
            }
        }
    }

    for symbol in &analysis.symbols {
        collect_symbol_ranges(symbol, key, &mut ranges);
    }

    if let DefinitionKey::Builtin { full_name } = key {
        let short = full_name.rsplit('.').next().unwrap_or(full_name.as_str());
        for token in &analysis.parsed.tokens {
            if !is_ident_token(token.kind) {
                continue;
            }
            if token.text(analysis.source.text()) != short {
                continue;
            }
            let resolved = analysis
                .references
                .iter()
                .find(|reference| reference.range.contains(token.range.start))
                .and_then(|reference| reference.key.as_ref());
            if resolved.is_none() || resolved == Some(key) {
                ranges.push(token.range);
            }
        }
    }

    let mut seen = HashSet::new();
    ranges
        .into_iter()
        .filter(|range| !range.is_empty())
        .filter(|range| seen.insert(*range))
        .map(|range| (analysis.uri.clone(), analysis.source.range(range)))
        .collect()
}

fn collect_symbol_ranges(symbol: &SymbolInfo, key: &DefinitionKey, output: &mut Vec<TextRange>) {
    if &symbol.key == key {
        output.push(symbol.name_range);
    }
    for child in &symbol.children {
        collect_symbol_ranges(child, key, output);
    }
}

fn current_file_ranges(ctx: &FeatureContext<'_>, key: &DefinitionKey) -> Vec<TextRange> {
    let mut ranges = Vec::new();
    for reference in &ctx.analysis.references {
        if reference.key.as_ref() == Some(key) {
            ranges.push(reference.range);
        }
    }
    for symbol in &ctx.analysis.symbols {
        collect_symbol_ranges(symbol, key, &mut ranges);
    }
    if matches!(key, DefinitionKey::Builtin { .. }) {
        if let Some(name) = key_name(ctx, key) {
            scan_word_tokens(ctx.analysis, &name, &mut ranges);
        }
    }
    let mut seen = HashSet::new();
    ranges
        .into_iter()
        .filter(|range| !range.is_empty())
        .filter(|range| seen.insert(*range))
        .collect()
}

fn write_ranges(ctx: &FeatureContext<'_>, key: &DefinitionKey) -> HashSet<TextRange> {
    let mut found = Vec::new();
    for symbol in &ctx.analysis.symbols {
        collect_symbol_ranges(symbol, key, &mut found);
    }
    let mut ranges: HashSet<TextRange> = found.into_iter().collect();
    if let Some(location) = resolve_location(ctx, key) {
        if location.uri == ctx.analysis.uri {
            ranges.insert(ctx.analysis.source.text_range(location.range));
        }
    }
    ranges
}

fn scan_word_tokens(analysis: &AnalyzedFile, word: &str, output: &mut Vec<TextRange>) {
    if word.is_empty() {
        return;
    }
    for token in &analysis.parsed.tokens {
        if !is_ident_token(token.kind) {
            continue;
        }
        if token.text(analysis.source.text()) == word {
            output.push(token.range);
        }
    }
}

// ---------------------------------------------------------------------------
// Rename validation and collision checks
// ---------------------------------------------------------------------------

fn uri_is_writable(ctx: &FeatureContext<'_>, uri: &Url) -> bool {
    let Ok(path) = uri.to_file_path() else {
        return false;
    };
    // Immutable compiler cache bundles are never writable, even when opened.
    if let Some(cache) = package_cache_dir() {
        if path.starts_with(&cache) {
            return false;
        }
    }
    if ctx.host.document_text(uri).is_some() {
        return true;
    }
    if ctx.host.is_workspace_path(&path) {
        return true;
    }
    false
}

fn key_is_writable(ctx: &FeatureContext<'_>, key: &DefinitionKey) -> bool {
    match key {
        DefinitionKey::Builtin { .. } | DefinitionKey::Module { .. } => false,
        DefinitionKey::Local { uri, .. } | DefinitionKey::TopLevel { uri, .. } => {
            uri_is_writable(ctx, uri)
        }
    }
}

fn valid_new_name(old: &str, new: &str) -> bool {
    if new.is_empty() || new == "_" || new == old {
        return false;
    }
    if new.chars().any(char::is_whitespace) {
        return false;
    }
    if Keyword::from_str(new).is_some() {
        return false;
    }
    if !new
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '!' || ch == '$')
    {
        return false;
    }

    let old_upper = old.chars().next().is_some_and(|ch| ch.is_ascii_uppercase());
    let new_upper = new.chars().next().is_some_and(|ch| ch.is_ascii_uppercase());
    if old_upper != new_upper {
        return false;
    }

    if old_upper {
        // Type and tag names are plain upper-camel identifiers.
        return new
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    }

    // Lowercase names may end in `!`; effectfulness must be preserved.
    if old.ends_with('!') != new.ends_with('!') {
        return false;
    }
    if new.trim_end_matches('!').contains('!') {
        return false;
    }
    // `$` is part of a variable name, so a rename must not add or remove it
    // (that would turn a constant into a variable, or vice versa).
    if new.contains('$') && !new.starts_with('$') {
        return false;
    }
    if new.starts_with('$') != old.starts_with('$') {
        return false;
    }
    true
}

fn has_collision(ctx: &FeatureContext<'_>, key: &DefinitionKey, new_name: &str) -> bool {
    match key {
        DefinitionKey::Local { uri, def_start } => with_analysis(ctx, uri, |analysis| {
            analysis
                .visible_bindings(*def_start)
                .into_iter()
                .any(|binding| binding.name == new_name && &binding.key != key)
                || analysis
                    .symbols
                    .iter()
                    .any(|symbol| &symbol.key != key && symbol.name == new_name)
        })
        .unwrap_or(true),
        DefinitionKey::TopLevel {
            uri,
            name,
            container,
        } => with_analysis(ctx, uri, |analysis| {
            if find_symbol(&analysis.symbols, new_name, container.as_deref()).is_some() {
                return true;
            }
            // A top-level name also collides with a value of the same name in
            // the same file.
            container.is_none()
                && analysis
                    .symbols
                    .iter()
                    .any(|symbol| symbol.name == new_name && symbol.name != *name)
        })
        .unwrap_or(true),
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn find_symbol<'a>(
    symbols: &'a [SymbolInfo],
    name: &str,
    container: Option<&str>,
) -> Option<&'a SymbolInfo> {
    for symbol in symbols {
        let container_matches = match container {
            Some(container) => symbol.container.as_deref() == Some(container),
            None => symbol.container.is_none(),
        };
        if symbol.name == name && container_matches {
            return Some(symbol);
        }
        // A container may be implicit on nested symbols.  When the requested
        // container names this symbol, look for the member among its children
        // without requiring an explicit `container` field.
        if container == Some(symbol.name.as_str()) {
            let explicit = find_symbol(&symbol.children, name, Some(symbol.name.as_str()));
            if let Some(found) = explicit.or_else(|| find_symbol(&symbol.children, name, None)) {
                return Some(found);
            }
        }
        if let Some(found) = find_symbol(&symbol.children, name, container) {
            return Some(found);
        }
    }
    None
}

fn find_symbol_at_offset(symbols: &[SymbolInfo], offset: u32) -> Option<&SymbolInfo> {
    for symbol in symbols {
        if symbol.name_range.contains_inclusive(offset) {
            return Some(symbol);
        }
        if let Some(found) = find_symbol_at_offset(&symbol.children, offset) {
            return Some(found);
        }
    }
    None
}

/// The declaration range for a local key: prefer the symbol table, falling
/// back to lexical scope bindings (some analyses put locals only in scopes).
fn declaration_range_at(analysis: &AnalyzedFile, offset: u32) -> Option<TextRange> {
    if let Some(symbol) = find_symbol_at_offset(&analysis.symbols, offset) {
        return Some(symbol.name_range);
    }
    analysis
        .scopes
        .iter()
        .flat_map(|scope| scope.bindings.iter())
        .find(|binding| binding.name_range.start == offset)
        .map(|binding| binding.name_range)
}

fn declaration_name_at(analysis: &AnalyzedFile, offset: u32) -> Option<String> {
    if let Some(symbol) = find_symbol_at_offset(&analysis.symbols, offset) {
        return Some(symbol.name.clone());
    }
    analysis
        .scopes
        .iter()
        .flat_map(|scope| scope.bindings.iter())
        .find(|binding| binding.name_range.start == offset)
        .map(|binding| binding.name.clone())
        .or_else(|| {
            analysis
                .references
                .iter()
                .find(|reference| reference.range.start == offset)
                .map(|reference| reference.text.clone())
        })
}

fn range_is_empty(range: &Range) -> bool {
    range.start == range.end
}

fn dedupe_locations(locations: Vec<Location>) -> Vec<Location> {
    let mut seen = HashSet::new();
    let mut output: Vec<Location> = Vec::new();
    for location in locations {
        let id = (
            location.uri.as_str().to_string(),
            location.range.start.line,
            location.range.start.character,
            location.range.end.line,
            location.range.end.character,
        );
        if seen.insert(id) {
            output.push(location);
        }
    }
    sort_locations(&mut output);
    output
}

fn sort_locations(locations: &mut [Location]) {
    locations.sort_by(|a, b| {
        a.uri
            .as_str()
            .cmp(b.uri.as_str())
            .then_with(|| {
                (a.range.start.line, a.range.start.character)
                    .cmp(&(b.range.start.line, b.range.start.character))
            })
            .then_with(|| {
                (a.range.end.line, a.range.end.character)
                    .cmp(&(b.range.end.line, b.range.end.character))
            })
    });
}

fn dedupe_highlights(highlights: &mut Vec<DocumentHighlight>) {
    let mut seen = HashSet::new();
    highlights.retain(|highlight| {
        seen.insert((
            highlight.range.start.line,
            highlight.range.start.character,
            highlight.range.end.line,
            highlight.range.end.character,
        ))
    });
    highlights.sort_by(|a, b| {
        (a.range.start.line, a.range.start.character)
            .cmp(&(b.range.start.line, b.range.start.character))
            .then_with(|| {
                (a.range.end.line, a.range.end.character)
                    .cmp(&(b.range.end.line, b.range.end.character))
            })
    });
}

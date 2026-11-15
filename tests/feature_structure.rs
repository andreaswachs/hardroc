//! Integration tests for document structure features: symbols, folding,
//! selection ranges, semantic tokens, links, and code lenses.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use lsp_types::{
    CodeLensParams, DocumentLinkParams, DocumentSymbol, DocumentSymbolParams,
    DocumentSymbolResponse, FoldingRangeKind, FoldingRangeParams, Position, Range, SelectionRange,
    SelectionRangeParams, SemanticTokensParams, TextDocumentIdentifier, Url, WorkspaceSymbolParams,
};

use hardroc::analysis::index::Export;
use hardroc::analysis::{
    AnalysisConfig, AnalysisHost, AnalyzedFile, DefinitionKey, ModuleKind, ModuleSummary,
    PackageInfo, PackageKind, Reference, ResolvedImport, SymbolInfo, SymbolKind, WorkspaceIndex,
};
use hardroc::config::Config;
use hardroc::features::structure;
use hardroc::features::FeatureContext;
use hardroc::syntax::ast::{
    Dependency, DependencySource, Expr, ExprKind, HeaderKind, Import, ImportPath, Item, ItemKind,
    Module, ModuleHeader, Name, ParsedFile,
};
use hardroc::syntax::token::{Token, TokenKind};
use hardroc::text::{SourceFile, TextRange};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fixture(relative: &str) -> (Url, PathBuf, String) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/structure")
        .join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let uri = Url::from_file_path(&path).expect("fixture uri");
    (uri, path, text)
}

fn nth_offset(text: &str, needle: &str, occurrence: usize) -> u32 {
    let mut from = 0;
    for index in 0..=occurrence {
        let found = text[from..]
            .find(needle)
            .unwrap_or_else(|| panic!("needle {needle:?} occurrence {occurrence}"));
        if index == occurrence {
            return (from + found) as u32;
        }
        from += found + needle.len();
    }
    unreachable!()
}

fn range_of(text: &str, needle: &str, occurrence: usize) -> TextRange {
    let start = nth_offset(text, needle, occurrence);
    TextRange::new(start, start + needle.len() as u32)
}

fn token(kind: TokenKind, range: TextRange) -> Token {
    Token::new(kind, range)
}

fn empty_parsed() -> ParsedFile {
    ParsedFile {
        module: Module::empty(),
        tokens: Vec::new(),
        diagnostics: Vec::new(),
    }
}

fn parsed(module: Module, tokens: Vec<Token>) -> ParsedFile {
    ParsedFile {
        module,
        tokens,
        diagnostics: Vec::new(),
    }
}

fn analyzed(
    uri: Url,
    text: &str,
    parsed: ParsedFile,
    symbols: Vec<SymbolInfo>,
    references: Vec<Reference>,
    imports: Vec<ResolvedImport>,
) -> AnalyzedFile {
    let module_name = uri
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or("main")
        .trim_end_matches(".roc")
        .to_string();
    AnalyzedFile {
        uri: uri.clone(),
        source: Arc::new(SourceFile::new(uri, text.to_string())),
        parsed: Arc::new(parsed),
        module_name,
        module_kind: ModuleKind::Module,
        symbols,
        references,
        scopes: Vec::new(),
        imports,
        diagnostics: Vec::new(),
        signatures: HashMap::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn symbol(
    uri: &Url,
    name: &str,
    kind: SymbolKind,
    name_range: TextRange,
    full_range: TextRange,
    container: Option<&str>,
    detail: Option<&str>,
    children: Vec<SymbolInfo>,
) -> SymbolInfo {
    SymbolInfo {
        name: name.to_string(),
        kind,
        name_range,
        full_range,
        detail: detail.map(str::to_string),
        doc: None,
        container: container.map(str::to_string),
        key: DefinitionKey::TopLevel {
            uri: uri.clone(),
            name: name.to_string(),
            container: container.map(str::to_string),
        },
        scope: 0,
        children,
    }
}

fn reference(
    uri: &Url,
    text: &str,
    kind: SymbolKind,
    range: TextRange,
    container: Option<&str>,
) -> Reference {
    Reference {
        range,
        text: text.to_string(),
        key: Some(DefinitionKey::TopLevel {
            uri: uri.clone(),
            name: text.to_string(),
            container: container.map(str::to_string),
        }),
        kind,
    }
}

fn import_roc(raw: &str, range: TextRange) -> Import {
    Import {
        path: ImportPath::Roc {
            raw: raw.to_string(),
            segments: vec![Name::new(raw, range)],
            range,
        },
        alias: None,
        ty: None,
        exposing: Vec::new(),
        range,
    }
}

fn resolved(import: Import, file_uri: Option<Url>) -> ResolvedImport {
    let qualifier = import.path.raw().to_string();
    ResolvedImport {
        qualifier,
        import,
        target: None,
        file_uri,
        exposing: Vec::new(),
        nested: Vec::new(),
        error: None,
    }
}

fn export(
    name: &str,
    kind: SymbolKind,
    selection_range: TextRange,
    full_range: TextRange,
    children: Vec<Export>,
) -> Export {
    Export {
        name: name.to_string(),
        kind,
        detail: None,
        doc: None,
        selection_range,
        full_range,
        container: None,
        key: None,
        children,
    }
}

fn module_summary(
    uri: Url,
    path: PathBuf,
    name: &str,
    exports: Vec<Export>,
    package: Option<&str>,
    package_root: Option<PathBuf>,
) -> ModuleSummary {
    ModuleSummary {
        uri,
        path,
        name: name.to_string(),
        kind: ModuleKind::Module,
        exports,
        dependencies: Vec::new(),
        header_kind: None,
        header_range: Some(TextRange::empty(0)),
        doc: None,
        package: package.map(str::to_string),
        package_root,
    }
}

fn contains_position(range: Range, position: Position) -> bool {
    position_le(range.start, position) && position_le(position, range.end)
}

fn position_le(a: Position, b: Position) -> bool {
    a.line < b.line || (a.line == b.line && a.character <= b.character)
}

fn range_contains(outer: Range, inner: Range) -> bool {
    position_le(outer.start, inner.start) && position_le(inner.end, outer.end)
}

fn ctx_for<'a>(
    host: &'a AnalysisHost,
    analysis: &'a AnalyzedFile,
    config: &'a Config,
) -> FeatureContext<'a> {
    FeatureContext::new(host, analysis, config)
}

fn document_symbol_params(uri: Url) -> DocumentSymbolParams {
    DocumentSymbolParams {
        text_document: TextDocumentIdentifier { uri },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}

fn folding_params(uri: Url) -> FoldingRangeParams {
    FoldingRangeParams {
        text_document: TextDocumentIdentifier { uri },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}

fn selection_params(uri: Url, positions: Vec<Position>) -> SelectionRangeParams {
    SelectionRangeParams {
        text_document: TextDocumentIdentifier { uri },
        positions,
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}

fn semantic_params(uri: Url) -> SemanticTokensParams {
    SemanticTokensParams {
        text_document: TextDocumentIdentifier { uri },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}

fn link_params(uri: Url) -> DocumentLinkParams {
    DocumentLinkParams {
        text_document: TextDocumentIdentifier { uri },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}

fn lens_params(uri: Url) -> CodeLensParams {
    CodeLensParams {
        text_document: TextDocumentIdentifier { uri },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}

fn workspace_symbol_params(query: &str) -> WorkspaceSymbolParams {
    WorkspaceSymbolParams {
        query: query.to_string(),
        partial_result_params: Default::default(),
        work_done_progress_params: Default::default(),
    }
}

fn find_document_symbol<'a>(
    symbols: &'a [DocumentSymbol],
    name: &str,
) -> Option<&'a DocumentSymbol> {
    for symbol in symbols {
        if symbol.name == name {
            return Some(symbol);
        }
        if let Some(children) = &symbol.children {
            if let Some(found) = find_document_symbol(children, name) {
                return Some(found);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Document symbols
// ---------------------------------------------------------------------------

#[test]
fn document_symbols_nest_color_methods() {
    let text = "module [Color]\n\nColor : [Red, Green, Blue]\n\nfmt : Color -> Str\nfmt = |color| \"\"\n\nparse : Str -> Color\nparse = |input| Color.Red\n";
    let uri = Url::parse("file:///tmp/structure/Color.roc").unwrap();

    let color_name = range_of(text, "Color", 1);
    let fmt_name = range_of(text, "fmt", 0);
    let parse_name = range_of(text, "parse", 0);
    let fmt = symbol(
        &uri,
        "fmt",
        SymbolKind::Method,
        fmt_name,
        TextRange::new(
            nth_offset(text, "fmt :", 0),
            nth_offset(text, "|color| \"\"", 0) + 11,
        ),
        Some("Color"),
        Some("Color -> Str"),
        Vec::new(),
    );
    let parse = symbol(
        &uri,
        "parse",
        SymbolKind::Method,
        parse_name,
        TextRange::new(
            nth_offset(text, "parse :", 0),
            nth_offset(text, "Color.Red", 0) + 9,
        ),
        Some("Color"),
        None,
        Vec::new(),
    );
    let color = symbol(
        &uri,
        "Color",
        SymbolKind::Type,
        color_name,
        TextRange::new(color_name.start, nth_offset(text, "fmt :", 0)),
        None,
        Some("[Red, Green, Blue]"),
        vec![fmt, parse],
    );
    let analysis = analyzed(
        uri.clone(),
        text,
        empty_parsed(),
        vec![color],
        Vec::new(),
        Vec::new(),
    );
    let host = AnalysisHost::new(PathBuf::from("/tmp/structure"), AnalysisConfig::default());
    let config = Config::default();
    let ctx = ctx_for(&host, &analysis, &config);

    let response = structure::document_symbols(&ctx, &document_symbol_params(uri))
        .expect("nested document symbols");
    let DocumentSymbolResponse::Nested(symbols) = response else {
        panic!("expected nested symbols");
    };
    assert_eq!(symbols.len(), 1, "one module root");
    let root = &symbols[0];
    assert_eq!(root.kind, lsp_types::SymbolKind::MODULE);

    let color_symbol = find_document_symbol(root.children.as_deref().unwrap_or(&[]), "Color")
        .expect("Color type symbol");
    assert_eq!(color_symbol.kind, lsp_types::SymbolKind::STRUCT);
    let methods = color_symbol.children.as_deref().unwrap_or(&[]);
    assert_eq!(methods.len(), 2);
    let names: Vec<&str> = methods.iter().map(|method| method.name.as_str()).collect();
    assert!(names.contains(&"fmt"));
    assert!(names.contains(&"parse"));
    let fmt_symbol = methods.iter().find(|method| method.name == "fmt").unwrap();
    assert_eq!(fmt_symbol.kind, lsp_types::SymbolKind::METHOD);
    assert_eq!(fmt_symbol.detail.as_deref(), Some("Color -> Str"));
    assert!(range_contains(fmt_symbol.range, fmt_symbol.selection_range));
}

// ---------------------------------------------------------------------------
// Folding ranges
// ---------------------------------------------------------------------------

#[test]
fn folding_ranges_are_multiline_and_cover_comments_literals_blocks_and_imports() {
    let text = "# top\n## doc one\n## doc two\n\"\"\"multi\nline\"\"\"\nfoo = {\n  a: 1,\n  b: 2,\n}\nimport Color\nimport json.Parser\n";
    let uri = Url::parse("file:///tmp/structure/folding.roc").unwrap();

    let comment = token(TokenKind::Comment, range_of(text, "# top", 0));
    let doc_one = token(TokenKind::DocComment, range_of(text, "## doc one", 0));
    let doc_two = token(TokenKind::DocComment, range_of(text, "## doc two", 0));
    let multiline_start = nth_offset(text, "\"\"\"multi", 0);
    let multiline_end = nth_offset(text, "line\"\"\"", 0) + "line\"\"\"".len() as u32;
    let multiline = token(
        TokenKind::MultilineString,
        TextRange::new(multiline_start, multiline_end),
    );
    let open_brace = token(TokenKind::LBrace, range_of(text, "{", 0));
    let close_brace = token(TokenKind::RBrace, range_of(text, "}", 0));

    let import_one_range = range_of(text, "import Color", 0);
    let import_two_range = range_of(text, "import json.Parser", 0);
    let module = Module {
        header: None,
        imports: vec![
            Import {
                path: ImportPath::Roc {
                    raw: "Color".to_string(),
                    segments: Vec::new(),
                    range: import_one_range,
                },
                alias: None,
                ty: None,
                exposing: Vec::new(),
                range: import_one_range,
            },
            Import {
                path: ImportPath::Roc {
                    raw: "json.Parser".to_string(),
                    segments: Vec::new(),
                    range: import_two_range,
                },
                alias: None,
                ty: None,
                exposing: Vec::new(),
                range: import_two_range,
            },
        ],
        items: Vec::new(),
        range: TextRange::new(0, text.len() as u32),
    };
    let analysis = analyzed(
        uri.clone(),
        text,
        parsed(
            module,
            vec![
                comment,
                doc_one,
                doc_two,
                multiline,
                open_brace,
                close_brace,
            ],
        ),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let host = AnalysisHost::new(PathBuf::from("/tmp/structure"), AnalysisConfig::default());
    let config = Config::default();
    let ctx = ctx_for(&host, &analysis, &config);

    let folds = structure::folding_ranges(&ctx, &folding_params(uri));
    assert!(!folds.is_empty());
    let line_count = text.lines().count() as u32;
    for fold in &folds {
        assert!(
            fold.end_line > fold.start_line,
            "single-line fold emitted: {fold:?}"
        );
        assert!(fold.start_line < line_count);
        assert!(fold.end_line < line_count);
    }

    assert!(folds.iter().any(|fold| fold.start_line == 1
        && fold.end_line == 2
        && fold.kind == Some(FoldingRangeKind::Comment)));
    assert!(folds
        .iter()
        .any(|fold| fold.start_line == 3 && fold.end_line == 4));
    assert!(folds
        .iter()
        .any(|fold| fold.start_line == 5 && fold.end_line == 8));
    assert!(folds.iter().any(|fold| fold.start_line == 9
        && fold.end_line == 10
        && fold.kind == Some(FoldingRangeKind::Imports)));
}

// ---------------------------------------------------------------------------
// Selection ranges
// ---------------------------------------------------------------------------

#[test]
fn selection_ranges_contain_the_cursor_and_nest() {
    let text = "foo = bar.baz\n";
    let uri = Url::parse("file:///tmp/structure/selection.roc").unwrap();
    let tokens = vec![
        token(TokenKind::LowerIdent, range_of(text, "foo", 0)),
        token(TokenKind::LowerIdent, range_of(text, "bar", 0)),
        token(TokenKind::LowerIdent, range_of(text, "baz", 0)),
    ];
    let analysis = analyzed(
        uri.clone(),
        text,
        parsed(Module::empty(), tokens),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let host = AnalysisHost::new(PathBuf::from("/tmp/structure"), AnalysisConfig::default());
    let config = Config::default();
    let ctx = ctx_for(&host, &analysis, &config);

    let bar_offset = nth_offset(text, "bar", 0);
    let position = analysis.source.position_at(bar_offset + 1);
    let ranges = structure::selection_ranges(&ctx, &selection_params(uri, vec![position]));
    assert_eq!(ranges.len(), 1);
    let first = &ranges[0];
    assert!(contains_position(first.range, position));

    let mut chain: Vec<Range> = Vec::new();
    let mut current: &SelectionRange = first;
    loop {
        chain.push(current.range);
        match &current.parent {
            Some(parent) => {
                assert!(
                    range_contains(parent.range, current.range),
                    "parent {parent:?} does not contain {:?}",
                    current.range
                );
                current = parent;
            }
            None => break,
        }
    }
    assert!(chain.len() >= 2, "expected token plus a wider range");
    assert!(chain.last().unwrap().start == Position::new(0, 0));
    assert!(chain
        .iter()
        .all(|range| contains_position(*range, position)));
}

// ---------------------------------------------------------------------------
// Semantic tokens
// ---------------------------------------------------------------------------

#[test]
fn semantic_tokens_are_sorted_non_overlapping_and_classified() {
    let text = "## docs\nColor : [Red]\nfmt = |c| 42\nname = \"roc\"\nlong = \"\"\"a\nb\"\"\"\n";
    let uri = Url::parse("file:///tmp/structure/tokens.roc").unwrap();

    let color_range = range_of(text, "Color", 0);
    let fmt_range = range_of(text, "fmt", 0);
    let c_start = nth_offset(text, "|c|", 0) + 1;
    let c_range = TextRange::new(c_start, c_start + 1);
    let number_range = range_of(text, "42", 0);
    let name_range = range_of(text, "name", 0);
    let string_range = range_of(text, "\"roc\"", 0);
    let long_range = range_of(text, "long", 0);
    let multiline_start = nth_offset(text, "\"\"\"a", 0);
    let multiline_end = nth_offset(text, "b\"\"\"", 0) + 4;

    let tokens = vec![
        token(TokenKind::DocComment, range_of(text, "## docs", 0)),
        token(TokenKind::UpperIdent, color_range),
        token(TokenKind::LowerIdent, fmt_range),
        token(TokenKind::LowerIdent, c_range),
        token(TokenKind::Number, number_range),
        token(TokenKind::LowerIdent, name_range),
        token(TokenKind::String, string_range),
        token(TokenKind::LowerIdent, long_range),
        token(
            TokenKind::MultilineString,
            TextRange::new(multiline_start, multiline_end),
        ),
    ];

    let color = symbol(
        &uri,
        "Color",
        SymbolKind::Type,
        color_range,
        range_of(text, "Color : [Red]", 0),
        None,
        None,
        Vec::new(),
    );
    let fmt = symbol(
        &uri,
        "fmt",
        SymbolKind::Function,
        fmt_range,
        range_of(text, "fmt = |c| 42", 0),
        None,
        None,
        vec![symbol(
            &uri,
            "c",
            SymbolKind::Parameter,
            c_range,
            c_range,
            Some("fmt"),
            None,
            Vec::new(),
        )],
    );
    let name = symbol(
        &uri,
        "name",
        SymbolKind::Constant,
        name_range,
        range_of(text, "name = \"roc\"", 0),
        None,
        None,
        Vec::new(),
    );
    let analysis = analyzed(
        uri.clone(),
        text,
        parsed(Module::empty(), tokens),
        vec![color, fmt, name],
        Vec::new(),
        Vec::new(),
    );
    let host = AnalysisHost::new(PathBuf::from("/tmp/structure"), AnalysisConfig::default());
    let config = Config::default();
    let ctx = ctx_for(&host, &analysis, &config);

    let result = structure::semantic_tokens(&ctx, &semantic_params(uri));
    assert!(!result.data.is_empty());

    let lines: Vec<&str> = text.lines().collect();
    let mut line = 0u32;
    let mut column = 0u32;
    let mut previous_end: Option<(u32, u32)> = None;
    let mut decoded: Vec<(u32, u32, u32, u32)> = Vec::new();
    for token in &result.data {
        line += token.delta_line;
        column = if token.delta_line == 0 {
            column + token.delta_start
        } else {
            token.delta_start
        };
        assert!(token.length > 0, "zero-length token: {token:?}");
        assert!(token.token_type < 22, "unknown token type: {token:?}");
        assert!(
            token.token_modifiers_bitset < (1 << 10),
            "modifier bitset out of range: {token:?}"
        );
        let line_width = lines
            .get(line as usize)
            .map(|line| line.encode_utf16().count() as u32)
            .unwrap_or(0);
        assert!(
            column + token.length <= line_width,
            "token past line end: {token:?} on {line:?} width {line_width}"
        );
        if let Some((previous_line, previous_column)) = previous_end {
            assert!(
                !(line == previous_line && column < previous_column),
                "overlapping tokens at line {line}"
            );
        }
        previous_end = Some((line, column + token.length));
        decoded.push((line, column, token.token_type, token.token_modifiers_bitset));
    }

    let doc = decoded
        .iter()
        .find(|(line, column, _, _)| *line == 0 && *column == 0)
        .expect("doc comment token");
    assert_eq!(doc.2, 16, "comment token type index");
    assert_ne!(doc.3 & (1 << 8), 0, "documentation modifier");

    let color = decoded
        .iter()
        .find(|(line, column, _, _)| *line == 1 && *column == 0)
        .expect("Color token");
    assert_eq!(color.2, 1, "type token index");
    assert_ne!(color.3 & 1, 0, "declaration modifier");

    let parameter = decoded
        .iter()
        .find(|(line, column, _, _)| *line == 2 && *column == 7)
        .expect("parameter token");
    assert_eq!(parameter.2, 7, "parameter token index");
    assert_ne!(parameter.3 & 1, 0, "declaration modifier");

    assert!(decoded
        .iter()
        .any(|(line, _, token_type, _)| *line == 4 && *token_type == 17));
    assert!(decoded
        .iter()
        .any(|(line, _, token_type, _)| *line == 5 && *token_type == 17));
}

// ---------------------------------------------------------------------------
// Document links
// ---------------------------------------------------------------------------

#[test]
fn document_links_resolve_existing_files_and_urls() {
    let (color_uri, _color_path, _color_text) = fixture("Color.roc");
    let (_data_uri, data_path, _data_text) = fixture("data.json");
    let data_uri = Url::from_file_path(&data_path).unwrap();
    let (pkg_main_uri, _pkg_main_path, _pkg_main_text) = fixture("pkg/main.roc");
    let main_uri = Url::parse("file:///tmp/structure/main.roc").unwrap();
    let mut text =
        String::from("module [main]\n\nimport Color\nimport \"data.json\" as data : Str\n\njson\n");
    text.push_str("https://example.com/basic-cli.tar.zst\n");

    let color_range = range_of(&text, "Color", 0);
    let file_range = range_of(&text, "\"data.json\"", 0);
    let mut file_import = Import {
        path: ImportPath::File {
            path: "data.json".to_string(),
            range: file_range,
        },
        alias: None,
        ty: None,
        exposing: Vec::new(),
        range: file_range,
    };
    file_import.alias = Some(Name::new("data", file_range));
    let imports = vec![
        resolved(import_roc("Color", color_range), Some(color_uri.clone())),
        resolved(file_import, Some(data_uri.clone())),
    ];

    let url_text = "https://example.com/basic-cli.tar.zst";
    let url_range = range_of(&text, url_text, 0);
    let shorthand_range = range_of(&text, "json", 1);
    let dependency = Dependency {
        shorthand: Name::new("json", shorthand_range),
        is_platform: false,
        source: DependencySource::RocVersion("0.1.0".to_string()),
        range: shorthand_range,
        source_range: shorthand_range,
    };
    let url_dependency = Dependency {
        shorthand: Name::new("cli", url_range),
        is_platform: false,
        source: DependencySource::Url(url_text.to_string()),
        range: url_range,
        source_range: url_range,
    };
    let header = ModuleHeader {
        kind: HeaderKind::App,
        name: None,
        provides: Vec::new(),
        dependencies: vec![dependency, url_dependency],
        exposes: Vec::new(),
        requires: None,
        provides_map: Vec::new(),
        hosted: Vec::new(),
        targets: None,
        range: TextRange::new(0, text.len() as u32),
    };
    let module = Module {
        header: Some(header),
        imports: Vec::new(),
        items: Vec::new(),
        range: TextRange::new(0, text.len() as u32),
    };
    let analysis = analyzed(
        main_uri.clone(),
        &text,
        parsed(module, Vec::new()),
        Vec::new(),
        Vec::new(),
        imports,
    );

    let host = AnalysisHost::new(PathBuf::from("/tmp/structure"), AnalysisConfig::default());
    let pkg_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/structure/pkg");
    let packages = vec![PackageInfo {
        shorthand: "json".to_string(),
        aliases: vec!["json".to_string()],
        source: DependencySource::RocVersion("0.1.0".to_string()),
        root: Some(pkg_dir.clone()),
        root_file: None,
        kind: PackageKind::Package,
        version: Some("0.1.0".to_string()),
        exposes: vec!["Parser".to_string()],
        modules: vec!["Parser".to_string()],
        dependencies: Vec::new(),
        header_range: None,
        error: None,
    }];
    let workspace_index = WorkspaceIndex::new(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/structure"),
        None,
        Vec::new(),
        packages,
    );
    host.replace_workspace(workspace_index);
    let config = Config::default();
    let ctx = ctx_for(&host, &analysis, &config);

    let links = structure::document_links(&ctx, &link_params(main_uri));
    assert!(links.len() >= 3, "links: {links:?}");
    let targets: Vec<&Url> = links
        .iter()
        .filter_map(|link| link.target.as_ref())
        .collect();
    assert!(targets.contains(&&color_uri));
    assert!(targets.contains(&&data_uri));
    assert!(targets.iter().any(|target| target.scheme() == "https"));
    assert!(targets.contains(&&pkg_main_uri));
    for link in &links {
        if let Some(target) = &link.target {
            if target.scheme() == "file" {
                assert!(
                    target.to_file_path().unwrap().is_file(),
                    "file link target must exist: {target}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Code lenses
// ---------------------------------------------------------------------------

#[test]
fn code_lenses_show_reference_counts_and_run_expects() {
    let text = "count : U64\ncount = 1\n\nmain = count + count\n\nexpect count == 1\n";
    let uri = Url::parse("file:///tmp/hardroc-tests/lenses/main.roc").unwrap();
    let count_name = range_of(text, "count", 0);
    let main_name = range_of(text, "main", 0);
    let expect_range = range_of(text, "expect count == 1", 0);
    let expr_range = range_of(text, "count == 1", 0);

    let count = symbol(
        &uri,
        "count",
        SymbolKind::Constant,
        count_name,
        TextRange::new(count_name.start, nth_offset(text, "count = 1", 0) + 9),
        None,
        Some("U64"),
        Vec::new(),
    );
    let main = symbol(
        &uri,
        "main",
        SymbolKind::Function,
        main_name,
        range_of(text, "main = count + count", 0),
        None,
        None,
        Vec::new(),
    );
    let references = vec![
        reference(
            &uri,
            "count",
            SymbolKind::Constant,
            range_of(text, "count", 1),
            None,
        ),
        reference(
            &uri,
            "count",
            SymbolKind::Constant,
            range_of(text, "count", 2),
            None,
        ),
    ];
    let expect_expr = Expr::new(ExprKind::Error, expr_range);
    let expect_item = Item {
        kind: ItemKind::Expect(hardroc::syntax::ast::Expect {
            expr: expect_expr,
            doc: None,
            range: expect_range,
        }),
        range: expect_range,
    };
    let module = Module {
        header: None,
        imports: Vec::new(),
        items: vec![expect_item],
        range: TextRange::new(0, text.len() as u32),
    };
    let analysis = analyzed(
        uri.clone(),
        text,
        parsed(module, Vec::new()),
        vec![count, main],
        references,
        Vec::new(),
    );
    let host = AnalysisHost::new(PathBuf::from("/tmp/structure"), AnalysisConfig::default());
    let config = Config::default();
    let ctx = ctx_for(&host, &analysis, &config);

    let lenses = structure::code_lenses(&ctx, &lens_params(uri));
    let reference_lens = lenses
        .iter()
        .find(|lens| {
            lens.command
                .as_ref()
                .is_some_and(|command| command.title == "2 references")
        })
        .expect("reference count lens");
    let command = reference_lens.command.as_ref().unwrap();
    assert_eq!(command.command, "hardroc.showReferences");
    assert!(reference_lens.range.start.line == 0);

    let test_lens = lenses
        .iter()
        .find(|lens| {
            lens.command
                .as_ref()
                .is_some_and(|command| command.command == "hardroc.runTest")
        })
        .expect("expect lens");
    let command = test_lens.command.as_ref().unwrap();
    assert_eq!(command.title, "Run test");
    assert!(command
        .arguments
        .as_ref()
        .is_some_and(|arguments| !arguments.is_empty()));
    assert_eq!(test_lens.range.start.line, 5);
}

// ---------------------------------------------------------------------------
// Workspace symbols
// ---------------------------------------------------------------------------

#[test]
fn workspace_symbols_include_workspace_and_package_items_and_filter() {
    let (color_uri, color_path, color_text) = fixture("Color.roc");
    let (parser_uri, parser_path, parser_text) = fixture("pkg/Parser.roc");

    let color_type = export(
        "Color",
        SymbolKind::Type,
        range_of(&color_text, "Color", 1),
        range_of(&color_text, "Color : [Red, Green, Blue]", 0),
        vec![export(
            "to_str",
            SymbolKind::Method,
            range_of(&color_text, "to_str", 0),
            range_of(&color_text, "to_str = |color| \"color\"", 0),
            Vec::new(),
        )],
    );
    let parser_module = export(
        "Parser",
        SymbolKind::Type,
        range_of(&parser_text, "Parser", 0),
        range_of(&parser_text, "Parser : { decode : Str -> Str }", 0),
        Vec::new(),
    );
    let decode = export(
        "decode",
        SymbolKind::Function,
        range_of(&parser_text, "decode", 1),
        range_of(&parser_text, "decode = |input| input", 0),
        Vec::new(),
    );

    let pkg_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/structure/pkg");
    let modules = vec![
        module_summary(
            color_uri.clone(),
            color_path,
            "Color",
            vec![color_type],
            None,
            None,
        ),
        module_summary(
            parser_uri,
            parser_path,
            "Parser",
            vec![parser_module, decode],
            Some("json"),
            Some(pkg_dir.clone()),
        ),
    ];
    let packages = vec![PackageInfo {
        shorthand: "json".to_string(),
        aliases: vec!["json".to_string()],
        source: DependencySource::RocVersion("0.1.0".to_string()),
        root: Some(pkg_dir),
        root_file: None,
        kind: PackageKind::Package,
        version: Some("0.1.0".to_string()),
        exposes: vec!["Parser".to_string()],
        modules: vec!["Parser".to_string()],
        dependencies: Vec::new(),
        header_range: None,
        error: None,
    }];

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/structure");
    let host = AnalysisHost::new(root.clone(), AnalysisConfig::default());
    host.replace_workspace(WorkspaceIndex::new(root, None, modules, packages));

    let main_uri = Url::parse("file:///tmp/structure/workspace-main.roc").unwrap();
    let main_text = "main = 1\n";
    let analysis = analyzed(
        main_uri,
        main_text,
        empty_parsed(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let config = Config::default();
    let ctx = ctx_for(&host, &analysis, &config);

    let all = structure::workspace_symbols(&ctx, &workspace_symbol_params(""));
    assert!(all
        .iter()
        .any(|symbol| symbol.name == "Color" && symbol.container_name.is_none()));
    assert!(all
        .iter()
        .any(|symbol| symbol.name == "Parser" && symbol.container_name.as_deref() == Some("json")));
    assert!(all
        .iter()
        .any(|symbol| symbol.name == "to_str" && symbol.kind == lsp_types::SymbolKind::METHOD));

    let filtered = structure::workspace_symbols(&ctx, &workspace_symbol_params("dec"));
    assert!(filtered
        .iter()
        .any(|symbol| symbol.name == "decode"
            && symbol.container_name.as_deref() == Some("json.Parser")));

    let package_query = structure::workspace_symbols(&ctx, &workspace_symbol_params("json.par"));
    assert!(package_query
        .iter()
        .any(|symbol| symbol.name == "decode"
            && symbol.container_name.as_deref() == Some("json.Parser")));

    let method_query = structure::workspace_symbols(&ctx, &workspace_symbol_params("to_str"));
    assert!(method_query
        .iter()
        .any(|symbol| symbol.name == "to_str" && symbol.kind == lsp_types::SymbolKind::METHOD));

    assert!(filtered
        .iter()
        .all(|symbol| symbol.name.to_lowercase().contains("dec")));
}

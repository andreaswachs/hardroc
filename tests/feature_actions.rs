//! Code action and range-formatting behavior.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use lsp_types::{
    CodeAction, CodeActionContext, CodeActionKind, CodeActionOrCommand, CodeActionParams, Position,
    Range, TextDocumentIdentifier, TextEdit, Url, WorkspaceEdit,
};

use hardroc::analysis::index::Export;
use hardroc::analysis::{
    AnalysisConfig, AnalysisDiagnostic, AnalysisHost, AnalyzedFile, DefinitionKey, DiagnosticFix,
    ModuleKind, ModuleSummary, ParamInfo, Reference, ResolvedImport, Signature, SuggestedEdit,
    SymbolInfo, SymbolKind, WorkspaceIndex,
};
use hardroc::config::Config;
use hardroc::features::actions;
use hardroc::features::FeatureContext;
use hardroc::syntax::ast::{
    Definition, Expr, ExprKind, HeaderKind, Import, ImportPath, Item, ItemKind, Module,
    ModuleHeader, Name, ParsedFile, Pattern, PatternKind,
};
use hardroc::text::{SourceFile, TextRange};

struct Fixture {
    _dir: tempfile::TempDir,
    host: AnalysisHost,
    config: Config,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let host = AnalysisHost::new(dir.path().to_path_buf(), AnalysisConfig::default());
    Fixture {
        _dir: dir,
        host,
        config: Config::default(),
    }
}

fn uri() -> Url {
    Url::parse("file:///tmp/hardroc-actions/Main.roc").unwrap()
}

fn make_analysis(uri: Url, text: &str, module: Module) -> AnalyzedFile {
    let source = Arc::new(SourceFile::new(uri.clone(), text.to_string()));
    AnalyzedFile {
        uri,
        source,
        parsed: Arc::new(ParsedFile {
            module,
            tokens: Vec::new(),
            diagnostics: Vec::new(),
        }),
        module_name: "Main".to_string(),
        module_kind: ModuleKind::Module,
        symbols: Vec::new(),
        references: Vec::new(),
        scopes: Vec::new(),
        imports: Vec::new(),
        diagnostics: Vec::new(),
        signatures: HashMap::new(),
    }
}

fn offset_of(text: &str, needle: &str, occurrence: usize) -> u32 {
    text.match_indices(needle)
        .nth(occurrence)
        .unwrap_or_else(|| panic!("missing {needle} occurrence {occurrence}"))
        .0 as u32
}

fn range_of(text: &str, needle: &str, occurrence: usize) -> TextRange {
    let start = offset_of(text, needle, occurrence);
    TextRange::new(start, start + needle.len() as u32)
}

fn import(text: &str, raw: &str, occurrence: usize) -> Import {
    let line_start = offset_of(text, &format!("import {raw}"), occurrence);
    let line_end = text[line_start as usize..]
        .find('\n')
        .map(|offset| line_start + offset as u32)
        .unwrap_or(text.len() as u32);
    let path_start = line_start + "import ".len() as u32;
    Import {
        path: ImportPath::Roc {
            raw: raw.to_string(),
            segments: vec![Name::new(
                raw,
                TextRange::new(path_start, path_start + raw.len() as u32),
            )],
            range: TextRange::new(path_start, path_start + raw.len() as u32),
        },
        alias: None,
        ty: None,
        exposing: Vec::new(),
        range: TextRange::new(line_start, line_end),
    }
}

fn header(text: &str, kind: HeaderKind, end_needle: &str) -> ModuleHeader {
    let end = offset_of(text, end_needle, 0) + end_needle.len() as u32;
    ModuleHeader {
        kind,
        name: None,
        provides: Vec::new(),
        dependencies: Vec::new(),
        exposes: Vec::new(),
        requires: None,
        provides_map: Vec::new(),
        hosted: Vec::new(),
        targets: None,
        range: TextRange::new(0, end),
    }
}

fn module_summary(name: &str, exports: Vec<Export>) -> ModuleSummary {
    ModuleSummary {
        uri: Url::parse(&format!("file:///tmp/hardroc-actions/{name}.roc")).unwrap(),
        path: PathBuf::from(format!("/tmp/hardroc-actions/{name}.roc")),
        name: name.to_string(),
        kind: ModuleKind::Module,
        exports,
        dependencies: Vec::new(),
        header_kind: None,
        header_range: None,
        doc: None,
        package: None,
        package_root: None,
    }
}

fn export(name: &str) -> Export {
    Export {
        name: name.to_string(),
        kind: SymbolKind::Function,
        detail: None,
        doc: None,
        selection_range: TextRange::empty(0),
        full_range: TextRange::empty(0),
        container: None,
        key: None,
        children: Vec::new(),
    }
}

fn params(text: &str, at: Range) -> CodeActionParams {
    CodeActionParams {
        text_document: TextDocumentIdentifier {
            uri: text_document_uri(text),
        },
        range: at,
        context: CodeActionContext {
            diagnostics: Vec::new(),
            only: None,
            trigger_kind: None,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}

fn text_document_uri(_text: &str) -> Url {
    uri()
}

fn only(mut params: CodeActionParams, kind: CodeActionKind) -> CodeActionParams {
    params.context.only = Some(vec![kind]);
    params
}

fn action_titled<'a>(actions: &'a [CodeActionOrCommand], title: &str) -> &'a CodeAction {
    actions
        .iter()
        .find_map(|action| match action {
            CodeActionOrCommand::CodeAction(action) if action.title == title => Some(action),
            _ => None,
        })
        .unwrap_or_else(|| {
            let titles = actions
                .iter()
                .map(|action| match action {
                    CodeActionOrCommand::CodeAction(action) => action.title.clone(),
                    CodeActionOrCommand::Command(command) => command.title.clone(),
                })
                .collect::<Vec<_>>();
            panic!("missing action {title:?}; got {titles:?}")
        })
}

fn action_containing<'a>(actions: &'a [CodeActionOrCommand], needle: &str) -> &'a CodeAction {
    actions
        .iter()
        .find_map(|action| match action {
            CodeActionOrCommand::CodeAction(action) if action.title.contains(needle) => {
                Some(action)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing action containing {needle:?}"))
}

fn apply_edit(text: &str, source: &SourceFile, edit: &WorkspaceEdit, uri: &Url) -> String {
    let edits = edit
        .changes
        .as_ref()
        .and_then(|changes| changes.get(uri))
        .expect("edit for document");
    let mut byte_edits: Vec<(u32, u32, String)> = edits
        .iter()
        .map(|edit| {
            let range = source.text_range(edit.range);
            (range.start, range.end, edit.new_text.clone())
        })
        .collect();
    byte_edits.sort_by_key(|(start, _, _)| *start);

    let mut output = text.to_string();
    for (start, end, new_text) in byte_edits.into_iter().rev() {
        output.replace_range(start as usize..end as usize, &new_text);
    }
    output
}

fn apply_single(edit: &WorkspaceEdit, uri: &Url) -> TextEdit {
    edit.changes
        .as_ref()
        .and_then(|changes| changes.get(uri))
        .and_then(|edits| edits.first())
        .cloned()
        .expect("single edit")
}

#[test]
fn unknown_module_quick_fix_inserts_import_after_header() {
    let text = "app [main!] {\n\troc: \"x\",\n}\n\nmain = Json.encode({})\n";
    let uri = uri();
    let module = Module {
        header: Some(header(text, HeaderKind::App, "}\n")),
        imports: Vec::new(),
        items: Vec::new(),
        range: TextRange::new(0, text.len() as u32),
    };
    let mut analysis = make_analysis(uri.clone(), text, module);
    let json = range_of(text, "Json", 0);
    analysis.diagnostics.push(AnalysisDiagnostic::error(
        json,
        "unknown-module",
        "module `Json` could not be found",
    ));

    let fixture = fixture();
    fixture.host.replace_workspace(WorkspaceIndex::new(
        fixture._dir.path().to_path_buf(),
        None,
        vec![module_summary("Json", Vec::new())],
        Vec::new(),
    ));
    let ctx = FeatureContext::new(&fixture.host, &analysis, &fixture.config);
    let params = params(text, analysis.source.range(json));
    let actions = actions::code_actions(&ctx, &params);

    let action = action_titled(&actions, "Import `Json`");
    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
    assert_eq!(action.is_preferred, Some(true));
    assert!(action.diagnostics.is_some());
    let edit = action.edit.as_ref().expect("edit");
    let text_edit = apply_single(edit, &uri);
    assert!(text_edit.new_text.contains("import Json\n"));

    let updated = apply_edit(text, &analysis.source, edit, &uri);
    assert!(updated.contains("}\n\nimport Json\n\nmain = Json.encode"));
}

#[test]
fn organize_imports_sorts_dedupes_and_keeps_used_imports() {
    let text = "package [Main] {\n}\n\nimport Zebra\nimport Alpha\nimport Zebra\n\nmain = Zebra.line\nother = Alpha.value\n";
    let uri = uri();
    let package_header = header(text, HeaderKind::Package, "}\n");
    let imports = vec![
        import(text, "Zebra", 0),
        import(text, "Alpha", 0),
        import(text, "Zebra", 1),
    ];
    let module = Module {
        header: Some(package_header),
        imports,
        items: Vec::new(),
        range: TextRange::new(0, text.len() as u32),
    };
    let mut analysis = make_analysis(uri.clone(), text, module);
    analysis.references.push(Reference {
        range: range_of(text, "Zebra", 2),
        text: "Zebra".to_string(),
        key: Some(DefinitionKey::Builtin {
            full_name: "Zebra.line".to_string(),
        }),
        kind: SymbolKind::Module,
    });
    analysis.references.push(Reference {
        range: range_of(text, "Alpha", 1),
        text: "Alpha".to_string(),
        key: Some(DefinitionKey::Builtin {
            full_name: "Alpha.value".to_string(),
        }),
        kind: SymbolKind::Module,
    });

    let fixture = fixture();
    let ctx = FeatureContext::new(&fixture.host, &analysis, &fixture.config);
    let params = only(
        params(
            text,
            analysis.source.range(TextRange::new(0, text.len() as u32)),
        ),
        CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
    );
    let actions = actions::code_actions(&ctx, &params);
    let action = action_titled(&actions, "Organize imports");
    let edit = action.edit.as_ref().expect("organize edit");
    let text_edit = apply_single(edit, &uri);
    assert_eq!(text_edit.new_text, "import Alpha\nimport Zebra\n");

    let updated = apply_edit(text, &analysis.source, edit, &uri);
    assert!(
        updated.contains("import Alpha\nimport Zebra\n\nmain = Zebra.line\nother = Alpha.value")
    );
    assert!(!updated.contains("import Zebra\nimport Zebra"));
}

#[test]
fn organize_imports_is_idempotent() {
    let text = "package [Main] {\n}\n\nimport Alpha\nimport Zebra\n\nmain = Zebra.line\nother = Alpha.value\n";
    let uri = uri();
    let module = Module {
        header: Some(header(text, HeaderKind::Package, "}\n")),
        imports: vec![import(text, "Alpha", 0), import(text, "Zebra", 0)],
        items: Vec::new(),
        range: TextRange::new(0, text.len() as u32),
    };
    let mut analysis = make_analysis(uri, text, module);
    analysis.references.push(Reference {
        range: range_of(text, "Zebra", 1),
        text: "Zebra".to_string(),
        key: None,
        kind: SymbolKind::Module,
    });
    analysis.references.push(Reference {
        range: range_of(text, "Alpha", 1),
        text: "Alpha".to_string(),
        key: None,
        kind: SymbolKind::Module,
    });

    let fixture = fixture();
    let ctx = FeatureContext::new(&fixture.host, &analysis, &fixture.config);
    let params = only(
        params(
            text,
            analysis.source.range(TextRange::new(0, text.len() as u32)),
        ),
        CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
    );
    let actions = actions::code_actions(&ctx, &params);
    assert!(actions.iter().all(|action| match action {
        CodeActionOrCommand::CodeAction(action) => action.title != "Organize imports",
        _ => true,
    }));
}

#[test]
fn add_annotation_uses_concrete_signature() {
    let text = "add = |a, b| a + b\n";
    let uri = uri();
    let name = Name::new("add", range_of(text, "add", 0));
    let definition = Definition {
        pattern: Pattern::new(
            PatternKind::Name {
                name: name.clone(),
                is_var: false,
            },
            name.range,
        ),
        annotation: None,
        value: Expr::new(ExprKind::Error, TextRange::empty(name.range.end)),
        is_var: false,
        doc: None,
        range: TextRange::new(0, text.len() as u32),
        equals_range: range_of(text, "=", 0),
    };
    let item_range = TextRange::new(0, text.len() as u32);
    let module = Module {
        header: None,
        imports: Vec::new(),
        items: vec![Item {
            kind: ItemKind::Definition(definition),
            range: item_range,
        }],
        range: item_range,
    };
    let key = DefinitionKey::TopLevel {
        uri: uri.clone(),
        name: "add".to_string(),
        container: None,
    };
    let mut analysis = make_analysis(uri.clone(), text, module);
    analysis.symbols.push(SymbolInfo {
        name: "add".to_string(),
        kind: SymbolKind::Function,
        name_range: name.range,
        full_range: item_range,
        detail: None,
        doc: None,
        container: None,
        key: key.clone(),
        scope: 0,
        children: Vec::new(),
    });
    analysis.signatures.insert(
        key.clone(),
        Signature {
            name: "add".to_string(),
            params: vec![
                ParamInfo {
                    name: "a".to_string(),
                    ty: Some("I64".to_string()),
                    name_range: TextRange::empty(0),
                },
                ParamInfo {
                    name: "b".to_string(),
                    ty: Some("I64".to_string()),
                    name_range: TextRange::empty(0),
                },
            ],
            return_type: Some("I64".to_string()),
            display: "I64, I64 -> I64".to_string(),
            is_effectful: false,
            type_params: Vec::new(),
            doc: None,
            key,
        },
    );

    let fixture = fixture();
    let ctx = FeatureContext::new(&fixture.host, &analysis, &fixture.config);
    let params = only(
        params(text, analysis.source.range(name.range)),
        CodeActionKind::QUICKFIX,
    );
    let actions = actions::code_actions(&ctx, &params);
    let action = action_containing(&actions, "Add type annotation");
    let edit = action.edit.as_ref().expect("annotation edit");
    let text_edit = apply_single(edit, &uri);
    assert_eq!(text_edit.new_text, "add : I64, I64 -> I64\n");
    assert_eq!(text_edit.range.start, Position::new(0, 0));
}

#[test]
fn diagnostic_fix_edits_are_converted_to_workspace_edits() {
    let text = "value = Str.conat(\"a\", \"b\")\n";
    let uri = uri();
    let module = Module {
        header: None,
        imports: Vec::new(),
        items: Vec::new(),
        range: TextRange::new(0, text.len() as u32),
    };
    let mut analysis = make_analysis(uri.clone(), text, module);
    let typo = range_of(text, "conat", 0);
    let mut diagnostic =
        AnalysisDiagnostic::error(typo, "unknown-name", "`conat` is not defined in this scope");
    diagnostic.fixes.push(DiagnosticFix {
        title: "Change to `concat`".to_string(),
        edits: vec![SuggestedEdit {
            uri: uri.clone(),
            range: typo,
            new_text: "concat".to_string(),
        }],
    });
    analysis.diagnostics.push(diagnostic);

    let fixture = fixture();
    let ctx = FeatureContext::new(&fixture.host, &analysis, &fixture.config);
    let params = params(text, analysis.source.range(typo));
    let actions = actions::code_actions(&ctx, &params);
    let action = action_titled(&actions, "Change to `concat`");
    assert_eq!(action.is_preferred, Some(true));
    let edit = action.edit.as_ref().expect("fix edit");
    let updated = apply_edit(text, &analysis.source, edit, &uri);
    assert_eq!(updated, "value = Str.concat(\"a\", \"b\")\n");
}

#[test]
fn unknown_name_qualifies_imported_module_member() {
    let text = "import Str\n\nmain = concat(\"a\", \"b\")\n";
    let uri = uri();
    let str_import = import(text, "Str", 0);
    let module = Module {
        header: None,
        imports: vec![str_import.clone()],
        items: Vec::new(),
        range: TextRange::new(0, text.len() as u32),
    };
    let target = module_summary("Str", vec![export("concat")]);
    let mut analysis = make_analysis(uri.clone(), text, module);
    analysis.imports.push(ResolvedImport {
        qualifier: "Str".to_string(),
        import: str_import,
        target: Some(target),
        file_uri: None,
        exposing: Vec::new(),
        nested: Vec::new(),
        error: None,
    });
    let unknown = range_of(text, "concat", 0);
    analysis.diagnostics.push(AnalysisDiagnostic::error(
        unknown,
        "unknown-name",
        "`concat` is not defined in this scope",
    ));

    let fixture = fixture();
    let ctx = FeatureContext::new(&fixture.host, &analysis, &fixture.config);
    let params = params(text, analysis.source.range(unknown));
    let actions = actions::code_actions(&ctx, &params);

    let qualify = action_titled(&actions, "Qualify as `Str.concat`");
    let edit = qualify.edit.as_ref().expect("qualification edit");
    let updated = apply_edit(text, &analysis.source, edit, &uri);
    assert_eq!(updated, "import Str\n\nmain = Str.concat(\"a\", \"b\")\n");

    let expose = action_containing(&actions, "Add `concat`");
    let expose_edit = expose.edit.as_ref().expect("exposing edit");
    let exposed = apply_edit(text, &analysis.source, expose_edit, &uri);
    assert!(exposed.starts_with("import Str exposing [concat]\n"));
}

#[test]
fn unknown_name_imports_top_level_item_from_another_module() {
    let text = "main = helper(1)\n";
    let uri = uri();
    let module = Module {
        header: None,
        imports: Vec::new(),
        items: Vec::new(),
        range: TextRange::new(0, text.len() as u32),
    };
    let mut analysis = make_analysis(uri.clone(), text, module);
    let unknown = range_of(text, "helper", 0);
    analysis.diagnostics.push(AnalysisDiagnostic::error(
        unknown,
        "unknown-name",
        "`helper` is not defined in this scope",
    ));

    let fixture = fixture();
    fixture.host.replace_workspace(WorkspaceIndex::new(
        fixture._dir.path().to_path_buf(),
        None,
        vec![module_summary("Utils", vec![export("helper")])],
        Vec::new(),
    ));
    let ctx = FeatureContext::new(&fixture.host, &analysis, &fixture.config);
    let params = params(text, analysis.source.range(unknown));
    let actions = actions::code_actions(&ctx, &params);

    let action = action_titled(&actions, "Import `Utils` exposing [`helper`]");
    let edit = action.edit.as_ref().expect("import edit");
    let updated = apply_edit(text, &analysis.source, edit, &uri);
    assert_eq!(
        updated,
        "import Utils exposing [helper]\n\nmain = helper(1)\n"
    );
}

#[test]
fn line_diff_edits_stays_inside_requested_range() {
    let source = "alpha  =  1\nbeta  =  2\ngamma  =  3\n";
    let formatted = "alpha = 10\nbeta = 2\ngamma = 3\n";
    let range = Range::new(Position::new(1, 0), Position::new(2, 0));
    let edits = actions::line_diff_edits(source, formatted, range).expect("edits");
    assert!(!edits.is_empty());
    for edit in &edits {
        assert_eq!(edit.range.start.line, 1);
        assert!(edit.range.end.line <= 2);
    }

    let source_file = SourceFile::new(uri(), source.to_string());
    let mut updated = source.to_string();
    let mut byte_edits: Vec<(u32, u32, String)> = edits
        .iter()
        .map(|edit| {
            let range = source_file.text_range(edit.range);
            (range.start, range.end, edit.new_text.clone())
        })
        .collect();
    byte_edits.sort_by_key(|(start, _, _)| *start);
    for (start, end, new_text) in byte_edits.into_iter().rev() {
        updated.replace_range(start as usize..end as usize, &new_text);
    }
    assert_eq!(updated, "alpha  =  1\nbeta = 2\ngamma  =  3\n");
}

#[test]
fn line_diff_edits_round_trips_full_range() {
    let source = "alpha  =  1\nbeta  =  2\ngamma  =  3\n";
    let formatted = "alpha = 1\nbeta = 2\ngamma = 3\n";
    let range = Range::new(Position::new(0, 0), Position::new(3, 0));
    let edits = actions::line_diff_edits(source, formatted, range).expect("edits");
    let source_file = SourceFile::new(uri(), source.to_string());
    let mut updated = source.to_string();
    let mut byte_edits: Vec<(u32, u32, String)> = edits
        .iter()
        .map(|edit| {
            let range = source_file.text_range(edit.range);
            (range.start, range.end, edit.new_text.clone())
        })
        .collect();
    byte_edits.sort_by_key(|(start, _, _)| *start);
    for (start, end, new_text) in byte_edits.into_iter().rev() {
        updated.replace_range(start as usize..end as usize, &new_text);
    }
    assert_eq!(updated, formatted);
}

#[test]
fn line_diff_edits_rejects_partial_changes_that_move_lines() {
    let source = "app [main!] {\n\troc: \"nightly-2026-11-09-9e85de7\",\n}\n\nmain! = |_| {Ok({})}\nsettings =   { a: 1 }\n";
    let formatted = "app [main!] {\n\troc: \"nightly-2026-11-09-9e85de7\",\n}\n\nmain! = |_| {\n\tOk({})\n}\n\nsettings = { a: 1 }\n";
    let range = Range::new(Position::new(4, 0), Position::new(6, 0));
    assert!(
        actions::line_diff_edits(source, formatted, range).is_none(),
        "a partial range must not apply only the deletion half of a moved change"
    );

    let full = Range::new(Position::new(0, 0), Position::new(7, 0));
    let edits = actions::line_diff_edits(source, formatted, full).expect("full edits");
    let source_file = SourceFile::new(uri(), source.to_string());
    let mut updated = source.to_string();
    let mut byte_edits: Vec<(u32, u32, String)> = edits
        .iter()
        .map(|edit| {
            let range = source_file.text_range(edit.range);
            (range.start, range.end, edit.new_text.clone())
        })
        .collect();
    byte_edits.sort_by_key(|(start, _, _)| *start);
    for (start, end, new_text) in byte_edits.into_iter().rev() {
        updated.replace_range(start as usize..end as usize, &new_text);
    }
    assert_eq!(updated, formatted);
}

#[test]
fn full_document_edit_ends_at_the_document_end() {
    let source = "app [main!] {\n\ttroc: \"nightly\",\n}\n";
    let formatted = "app [main!] {\n\ttroc: \"nightly\",\n}\n";
    assert!(actions::full_document_edit(source, formatted).is_none());
    let changed = "app [main!] {\n\ttroc: \"nightly\",\n}\n\nmain! = |_| {}\n";
    let edit = actions::full_document_edit(source, changed).expect("edit");
    let file = SourceFile::new(uri(), source.to_string());
    assert_eq!(edit.range.end, file.position_at(source.len() as u32));
}

#[test]
fn package_source_action_uses_a_server_command() {
    let temp = tempfile::tempdir().expect("temp workspace");
    let root = temp.path();
    std::fs::create_dir_all(root.join("lib")).expect("lib dir");
    std::fs::write(root.join("lib/main.roc"), "package [Widget] {}\n").expect("package");
    std::fs::write(root.join("lib/Widget.roc"), "module [x]\n\nx = 1\n").expect("module");
    let path = root.join("main.roc");
    let text =
        "app [main!] {\n\tlib: \"./lib/main.roc\",\n\troc: \"nightly\",\n}\n\nmain! = |_| {}\n";
    std::fs::write(&path, text).expect("app");
    let uri = Url::from_file_path(&path).expect("uri");
    let host = AnalysisHost::new(root.to_path_buf(), AnalysisConfig::default());
    let analysis = host.analyze(&uri).expect("analysis");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &analysis, &config);
    let dependency = analysis
        .parsed
        .module
        .header
        .as_ref()
        .and_then(|header| header.dependencies.first())
        .expect("dependency");
    let params = params(text, analysis.source.range(dependency.range));
    let actions = actions::code_actions(&ctx, &params);
    let action = actions
        .iter()
        .find_map(|action| match action {
            CodeActionOrCommand::Command(command)
                if command.title.contains("Open package source") =>
            {
                Some(command)
            }
            _ => None,
        })
        .expect("open package source action");
    assert_eq!(action.command, "hardroc.openPackageSource");
    let argument = action
        .arguments
        .as_ref()
        .and_then(|arguments| arguments.first())
        .and_then(|value| value.as_str())
        .expect("target argument");
    assert!(argument.ends_with("/lib/main.roc"), "{argument}");
}

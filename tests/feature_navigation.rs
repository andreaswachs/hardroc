//! Feature tests for navigation: goto, references, rename, and highlights.

use std::path::{Path, PathBuf};

use lsp_types::{
    DocumentChanges, DocumentHighlightKind, DocumentHighlightParams, GotoDefinitionParams,
    GotoDefinitionResponse, Location, PartialResultParams, Position, PrepareRenameResponse,
    ReferenceContext, ReferenceParams, RenameParams, TextDocumentEdit, TextDocumentIdentifier,
    TextDocumentPositionParams, TextEdit, Url, WorkDoneProgressParams, WorkspaceEdit,
};
use hardroc::analysis::index::{Export, ModuleKind, ModuleSummary};
use hardroc::analysis::{AnalysisConfig, AnalysisHost, DefinitionKey, SymbolKind};
use hardroc::config::Config;
use hardroc::features::hierarchy;
use hardroc::features::navigation::{self, NavigationKind};
use hardroc::features::FeatureContext;
use hardroc::syntax::ast::HeaderKind;
use hardroc::text::{SourceFile, TextRange};
use tempfile::tempdir;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("navigation")
        .join("workspace")
}

fn uri_for(path: &Path) -> Url {
    Url::from_file_path(path).expect("fixture path is absolute")
}

fn nth_offset(text: &str, needle: &str, occurrence: usize) -> u32 {
    text.match_indices(needle)
        .nth(occurrence)
        .unwrap_or_else(|| panic!("missing occurrence {occurrence} of {needle:?}"))
        .0 as u32
}

fn offset_of(text: &str, needle: &str) -> u32 {
    text.find(needle)
        .unwrap_or_else(|| panic!("missing {needle:?}")) as u32
}

fn position(source: &SourceFile, offset: u32) -> Position {
    source.position_at(offset)
}

fn text_document_position(uri: &Url, position: Position) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position,
    }
}

fn goto_params(uri: &Url, position: Position) -> GotoDefinitionParams {
    GotoDefinitionParams {
        text_document_position_params: text_document_position(uri, position),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn reference_params(uri: &Url, position: Position, include_declaration: bool) -> ReferenceParams {
    ReferenceParams {
        text_document_position: text_document_position(uri, position),
        context: ReferenceContext {
            include_declaration,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn rename_params(uri: &Url, position: Position, new_name: &str) -> RenameParams {
    RenameParams {
        text_document_position: text_document_position(uri, position),
        new_name: new_name.to_string(),
        work_done_progress_params: WorkDoneProgressParams::default(),
    }
}

fn highlight_params(uri: &Url, position: Position) -> DocumentHighlightParams {
    DocumentHighlightParams {
        text_document_position_params: text_document_position(uri, position),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn goto_locations(response: Option<GotoDefinitionResponse>) -> Vec<Location> {
    match response {
        Some(GotoDefinitionResponse::Array(locations)) => locations,
        Some(GotoDefinitionResponse::Scalar(location)) => vec![location],
        Some(GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|link| Location {
                uri: link.target_uri,
                range: link.target_selection_range,
            })
            .collect(),
        None => Vec::new(),
    }
}

fn workspace_edit_edits(edit: &WorkspaceEdit) -> Vec<(Url, TextEdit)> {
    let mut output = Vec::new();
    match &edit.document_changes {
        Some(DocumentChanges::Edits(documents)) => {
            for TextDocumentEdit {
                text_document,
                edits,
            } in documents
            {
                for edit in edits {
                    if let lsp_types::OneOf::Left(edit) = edit {
                        output.push((text_document.uri.clone(), (*edit).clone()));
                    }
                }
            }
        }
        _ => {
            if let Some(changes) = &edit.changes {
                for (uri, edits) in changes {
                    for edit in edits {
                        output.push((uri.clone(), (*edit).clone()));
                    }
                }
            }
        }
    }
    output
}

#[test]
fn goto_resolves_local_binding() {
    let root = fixture_root();
    let host = AnalysisHost::new(root.clone(), AnalysisConfig::default());
    let main_uri = uri_for(&root.join("main.roc"));
    let main = host.analyze(&main_uri).expect("main.roc is analyzed");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &main, &config);

    let text = main.source.text();
    let definition = nth_offset(text, "color", 0);
    let usage = nth_offset(text, "(color)", 0) + 1;

    let response = navigation::goto(
        &ctx,
        &goto_params(&main_uri, position(&main.source, usage)),
        NavigationKind::Definition,
    );
    let locations = goto_locations(response);
    assert_eq!(locations.len(), 1, "one definition for the local binding");
    assert_eq!(locations[0].uri, main_uri);
    assert_eq!(locations[0].range.start, position(&main.source, definition));
}

#[test]
fn goto_follows_qualified_use_to_method() {
    let root = fixture_root();
    let host = AnalysisHost::new(root.clone(), AnalysisConfig::default());
    let main_uri = uri_for(&root.join("main.roc"));
    let color_uri = uri_for(&root.join("Color.roc"));
    let main = host.analyze(&main_uri).expect("main.roc is analyzed");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &main, &config);

    let text = main.source.text();
    let qualified = offset_of(text, "Color.to_str(") + "Color.".len() as u32;
    let locations = goto_locations(navigation::goto(
        &ctx,
        &goto_params(&main_uri, position(&main.source, qualified)),
        NavigationKind::Definition,
    ));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, color_uri);

    let color_source = host.analyze(&color_uri).expect("Color.roc is analyzed");
    let line = locations[0].range.start.line as usize;
    assert!(
        color_source.source.line_text(line).contains("to_str"),
        "goto lands on the Color.to_str method, got line {:?}",
        color_source.source.line_text(line)
    );

    // An `exposing [to_str]` import resolves the unqualified use as well.
    let exposed = nth_offset(text, "to_str(color)", 1);
    let locations = goto_locations(navigation::goto(
        &ctx,
        &goto_params(&main_uri, position(&main.source, exposed)),
        NavigationKind::Definition,
    ));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, color_uri);
    let line = locations[0].range.start.line as usize;
    assert!(color_source.source.line_text(line).contains("to_str"));
}

#[test]
fn goto_type_definition_uses_annotation() {
    let root = fixture_root();
    let host = AnalysisHost::new(root.clone(), AnalysisConfig::default());
    let main_uri = uri_for(&root.join("main.roc"));
    let color_uri = uri_for(&root.join("Color.roc"));
    let main = host.analyze(&main_uri).expect("main.roc is analyzed");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &main, &config);

    let text = main.source.text();
    let value = offset_of(text, "default_color =");
    let locations = goto_locations(navigation::goto(
        &ctx,
        &goto_params(&main_uri, position(&main.source, value)),
        NavigationKind::TypeDefinition,
    ));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, color_uri);
    let color_source = SourceFile::new(
        color_uri.clone(),
        std::fs::read_to_string(root.join("Color.roc")).expect("Color.roc"),
    );
    let line = locations[0].range.start.line as usize;
    assert!(
        color_source.line_text(line).contains("Color"),
        "type definition lands on Color's declaration, got {:?}",
        color_source.line_text(line)
    );
}

#[test]
fn goto_implementation_lists_type_methods() {
    let root = fixture_root();
    let host = AnalysisHost::new(root.clone(), AnalysisConfig::default());
    let main_uri = uri_for(&root.join("main.roc"));
    let color_uri = uri_for(&root.join("Color.roc"));
    let main = host.analyze(&main_uri).expect("main.roc is analyzed");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &main, &config);

    let text = main.source.text();
    let annotation = offset_of(text, "default_color : Color") + "default_color : ".len() as u32;
    let locations = goto_locations(navigation::goto(
        &ctx,
        &goto_params(&main_uri, position(&main.source, annotation)),
        NavigationKind::Implementation,
    ));
    assert!(!locations.is_empty(), "Color has implementations");
    assert!(locations.iter().all(|location| location.uri == color_uri));

    let color_source = SourceFile::new(
        color_uri.clone(),
        std::fs::read_to_string(root.join("Color.roc")).expect("Color.roc"),
    );
    assert!(
        locations.iter().any(|location| {
            color_source
                .line_text(location.range.start.line as usize)
                .contains("to_str")
        }),
        "implementation includes to_str: {locations:?}"
    );
}

#[test]
fn goto_import_alias_lands_on_module_file() {
    let root = fixture_root();
    let host = AnalysisHost::new(root.clone(), AnalysisConfig::default());
    let alias_uri = uri_for(&root.join("Alias.roc"));
    let color_uri = uri_for(&root.join("Color.roc"));
    let alias = host.analyze(&alias_uri).expect("Alias.roc is analyzed");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &alias, &config);

    let text = alias.source.text();
    let qualifier = offset_of(text, "C.to_str(");
    let locations = goto_locations(navigation::goto(
        &ctx,
        &goto_params(&alias_uri, position(&alias.source, qualifier)),
        NavigationKind::Definition,
    ));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, color_uri);

    // The alias declaration itself points at the module file too.
    let declaration = offset_of(text, "import Color as C") + "import Color as ".len() as u32;
    let locations = goto_locations(navigation::goto(
        &ctx,
        &goto_params(&alias_uri, position(&alias.source, declaration)),
        NavigationKind::Definition,
    ));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, color_uri);
}

#[test]
fn goto_on_import_path_lands_on_module_file() {
    let root = fixture_root();
    let host = AnalysisHost::new(root.clone(), AnalysisConfig::default());
    let main_uri = uri_for(&root.join("main.roc"));
    let color_uri = uri_for(&root.join("Color.roc"));
    let main = host.analyze(&main_uri).expect("main.roc is analyzed");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &main, &config);

    let text = main.source.text();
    let imported = offset_of(text, "import Color") + "import ".len() as u32;
    let locations = goto_locations(navigation::goto(
        &ctx,
        &goto_params(&main_uri, position(&main.source, imported)),
        NavigationKind::Definition,
    ));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, color_uri);
}

#[test]
fn references_respect_include_declaration_and_cross_files() {
    let root = fixture_root();
    let host = AnalysisHost::new(root.clone(), AnalysisConfig::default());
    let main_uri = uri_for(&root.join("main.roc"));
    let color_uri = uri_for(&root.join("Color.roc"));
    let other_uri = uri_for(&root.join("Other.roc"));
    let main = host.analyze(&main_uri).expect("main.roc is analyzed");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &main, &config);

    let text = main.source.text();
    let qualified = offset_of(text, "Color.to_str(") + "Color.".len() as u32;
    let position = position(&main.source, qualified);
    let declaration = goto_locations(navigation::goto(
        &ctx,
        &goto_params(&main_uri, position),
        NavigationKind::Definition,
    ))
    .into_iter()
    .next()
    .expect("method declaration resolves");
    assert_eq!(declaration.uri, color_uri);

    let without = navigation::references(&ctx, &reference_params(&main_uri, position, false));
    assert!(
        without.iter().any(|location| location.uri == other_uri),
        "usage in Other.roc is a reference: {without:?}"
    );
    assert!(
        !without
            .iter()
            .any(|location| location.uri == color_uri
                && location.range.start == declaration.range.start),
        "declaration excluded when include_declaration is false"
    );

    let with = navigation::references(&ctx, &reference_params(&main_uri, position, true));
    assert!(
        with.iter()
            .any(|location| location.uri == color_uri
                && location.range.start == declaration.range.start),
        "declaration included when requested: {with:?}"
    );
    let mut sorted = with.clone();
    sorted.sort_by(|a, b| {
        a.uri.as_str().cmp(b.uri.as_str()).then_with(|| {
            (a.range.start.line, a.range.start.character)
                .cmp(&(b.range.start.line, b.range.start.character))
        })
    });
    assert_eq!(with, sorted, "references are sorted by uri and position");
}

#[test]
fn rename_edits_every_workspace_file() {
    let root = fixture_root();
    let host = AnalysisHost::new(root.clone(), AnalysisConfig::default());
    let main_uri = uri_for(&root.join("main.roc"));
    let color_uri = uri_for(&root.join("Color.roc"));
    let other_uri = uri_for(&root.join("Other.roc"));
    let main = host.analyze(&main_uri).expect("main.roc is analyzed");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &main, &config);

    let text = main.source.text();
    let qualified = offset_of(text, "Color.to_str(") + "Color.".len() as u32;
    let position = position(&main.source, qualified);

    let prepared = navigation::prepare_rename(&ctx, &text_document_position(&main_uri, position));
    match prepared {
        Some(PrepareRenameResponse::RangeWithPlaceholder { range, placeholder }) => {
            assert_eq!(placeholder, "to_str");
            assert_eq!(
                range.start,
                main.source
                    .position_at(offset_of(text, "Color.to_str(") + "Color.".len() as u32)
            );
        }
        other => panic!("expected range with placeholder, got {other:?}"),
    }

    let edit = navigation::rename(&ctx, &rename_params(&main_uri, position, "show"))
        .expect("workspace rename succeeds");
    let edits = workspace_edit_edits(&edit);
    assert!(edits.iter().any(|(uri, _)| uri == &color_uri));
    assert!(edits.iter().any(|(uri, _)| uri == &other_uri));
    assert!(edits.iter().any(|(uri, _)| uri == &main_uri));
    assert!(edits.iter().all(|(_, edit)| edit.new_text == "show"));

    // Invalid names are rejected without producing edits.
    for invalid in ["Color", "and", "_", "to str", "to_str"] {
        assert!(
            navigation::rename(&ctx, &rename_params(&main_uri, position, invalid)).is_none(),
            "rename to {invalid:?} must be rejected"
        );
    }
}

#[test]
fn rename_and_prepare_reject_builtins() {
    let root = fixture_root();
    let host = AnalysisHost::new(root.clone(), AnalysisConfig::default());
    let main_uri = uri_for(&root.join("main.roc"));
    let main = host.analyze(&main_uri).expect("main.roc is analyzed");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &main, &config);

    let text = main.source.text();
    let concat = offset_of(text, "Str.concat(") + "Str.".len() as u32;
    let position = position(&main.source, concat);
    assert!(
        navigation::prepare_rename(&ctx, &text_document_position(&main_uri, position)).is_none(),
        "builtins are not renameable"
    );
    assert!(
        navigation::rename(&ctx, &rename_params(&main_uri, position, "join")).is_none(),
        "rename never edits builtins"
    );
}

#[test]
fn highlights_return_all_same_key_occurrences() {
    let root = fixture_root();
    let host = AnalysisHost::new(root.clone(), AnalysisConfig::default());
    let main_uri = uri_for(&root.join("main.roc"));
    let main = host.analyze(&main_uri).expect("main.roc is analyzed");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &main, &config);

    let text = main.source.text();
    let expected_offsets: Vec<u32> = [
        nth_offset(text, "color = Color.red", 0),
        nth_offset(text, "Color.to_str(color)", 0) + "Color.to_str(".len() as u32,
        nth_offset(text, "to_str(color)", 1) + "to_str(".len() as u32,
        nth_offset(text, "Other.describe(color)", 0) + "Other.describe(".len() as u32,
        nth_offset(text, "helper(color)", 0) + "helper(".len() as u32,
    ]
    .into_iter()
    .collect();

    let cursor = expected_offsets[1];
    let highlights = navigation::highlights(
        &ctx,
        &highlight_params(&main_uri, position(&main.source, cursor)),
    );
    let mut ranges: Vec<Position> = highlights
        .iter()
        .map(|highlight| highlight.range.start)
        .collect();
    let mut expected: Vec<Position> = expected_offsets
        .iter()
        .map(|offset| position(&main.source, *offset))
        .collect();
    ranges.sort_by_key(|position| (position.line, position.character));
    expected.sort_by_key(|position| (position.line, position.character));

    assert_eq!(ranges, expected, "all `color` occurrences in main.roc");
    let declaration = highlights
        .iter()
        .find(|highlight| highlight.range.start == position(&main.source, expected_offsets[0]))
        .expect("declaration is highlighted");
    assert_eq!(declaration.kind, Some(DocumentHighlightKind::WRITE));
    assert!(highlights
        .iter()
        .filter(|highlight| highlight.range.start != declaration.range.start)
        .all(|highlight| highlight.kind == Some(DocumentHighlightKind::READ)));

    // The `color` parameter of `helper` is a different key and must not be
    // highlighted with the local in `main!`.
    let helper_param = nth_offset(text, "helper = |color|", 0) + "helper = |".len() as u32;
    assert!(
        !highlights
            .iter()
            .any(|highlight| highlight.range.start == position(&main.source, helper_param)),
        "unrelated local binding is not mixed in"
    );
}

#[test]
fn goto_package_module_and_read_only_rename() {
    let temp = tempdir().expect("temp directory");
    let root = temp.path().join("workspace");
    let deps = temp.path().join("deps");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&deps).unwrap();

    let main_text = "app [main!] {\n\troc: \"nightly\",\n}\n\nimport acme.Widget\n\nmain! = |_| Widget.render(1)\n";
    let widget_text = "Widget :: {}.{\n\trender : U64 -> Str\n\trender = |_| \"widget\"\n}\n";
    let main_path = root.join("main.roc");
    let widget_path = deps.join("Widget.roc");
    std::fs::write(&main_path, main_text).unwrap();
    std::fs::write(&widget_path, widget_text).unwrap();

    let main_uri = uri_for(&main_path);
    let widget_uri = uri_for(&widget_path);
    let render_offset = offset_of(widget_text, "render =");
    let render_range = TextRange::new(render_offset, render_offset + "render".len() as u32);

    let widget_module = ModuleSummary {
        uri: widget_uri.clone(),
        path: widget_path.clone(),
        name: "Widget".to_string(),
        kind: ModuleKind::Module,
        exports: vec![Export {
            name: "render".to_string(),
            kind: SymbolKind::Function,
            detail: None,
            doc: None,
            selection_range: render_range,
            full_range: render_range,
            container: Some("Widget".to_string()),
            key: Some(DefinitionKey::TopLevel {
                uri: widget_uri.clone(),
                name: "render".to_string(),
                container: Some("Widget".to_string()),
            }),
            children: Vec::new(),
        }],
        dependencies: Vec::new(),
        header_kind: None::<HeaderKind>,
        header_range: None,
        doc: None,
        package: Some("acme".to_string()),
        package_root: Some(deps.clone()),
    };
    let workspace = hardroc::analysis::index::WorkspaceIndex::new(
        root.clone(),
        Some(main_path.clone()),
        vec![widget_module],
        Vec::new(),
    );

    let host = AnalysisHost::new(root.clone(), AnalysisConfig::default());
    host.replace_workspace(workspace);
    host.set_document(main_uri.clone(), main_text.to_string());
    let main = host.analyze(&main_uri).expect("main.roc is analyzed");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &main, &config);

    let use_offset = offset_of(main_text, "Widget.render") + "Widget.".len() as u32;
    let locations = goto_locations(navigation::goto(
        &ctx,
        &goto_params(&main_uri, position(&main.source, use_offset)),
        NavigationKind::Definition,
    ));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, widget_uri);
    let widget_source = SourceFile::new(widget_uri.clone(), widget_text.to_string());
    let line = locations[0].range.start.line as usize;
    assert!(
        widget_source.line_text(line).contains("render"),
        "goto lands on the package method, got {:?}",
        widget_source.line_text(line)
    );

    let import_offset = offset_of(main_text, "import acme.Widget") + "import acme.".len() as u32;
    let locations = goto_locations(navigation::goto(
        &ctx,
        &goto_params(&main_uri, position(&main.source, import_offset)),
        NavigationKind::Definition,
    ));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, widget_uri);

    // Package sources live outside the workspace and are read-only.
    assert!(
        navigation::prepare_rename(
            &ctx,
            &text_document_position(&main_uri, position(&main.source, use_offset))
        )
        .is_none(),
        "package cache modules are not renameable"
    );
    assert!(navigation::rename(
        &ctx,
        &rename_params(&main_uri, position(&main.source, use_offset), "draw")
    )
    .is_none());
}

#[test]
fn call_hierarchy_follows_outgoing_and_incoming_calls() {
    let root = fixture_root();
    let host = AnalysisHost::new(root.clone(), AnalysisConfig::default());
    let main_uri = uri_for(&root.join("main.roc"));
    let main = host.analyze(&main_uri).expect("main.roc is analyzed");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &main, &config);

    let prepare_params = lsp_types::CallHierarchyPrepareParams {
        text_document_position_params: text_document_position(
            &main_uri,
            position(&main.source, offset_of(main.source.text(), "main! =")),
        ),
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    let item = hierarchy::prepare(&ctx, &prepare_params)
        .and_then(|items| items.into_iter().next())
        .expect("main! call hierarchy item");

    let outgoing = hierarchy::outgoing(
        &ctx,
        &lsp_types::CallHierarchyOutgoingCallsParams {
            item: item.clone(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .expect("outgoing calls");
    assert!(
        outgoing.iter().any(|call| call.to.name == "helper"),
        "main! should call helper: {outgoing:?}"
    );

    let helper = host
        .analyze(&main_uri)
        .expect("main analyzed")
        .symbols
        .iter()
        .find(|symbol| symbol.name == "helper")
        .cloned()
        .expect("helper symbol");
    let helper_item = lsp_types::CallHierarchyItem {
        name: helper.name.clone(),
        kind: lsp_types::SymbolKind::FUNCTION,
        tags: None,
        detail: None,
        uri: main_uri.clone(),
        range: main.source.range(helper.full_range),
        selection_range: main.source.range(helper.name_range),
        data: serde_json::to_value(&helper.key).ok(),
    };
    let incoming = hierarchy::incoming(
        &ctx,
        &lsp_types::CallHierarchyIncomingCallsParams {
            item: helper_item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .expect("incoming calls");
    assert!(
        incoming.iter().any(|call| call.from.name == "main!"),
        "helper should be called from main!: {incoming:?}"
    );
}

#[test]
fn rename_preserves_the_variable_dollar_prefix() {
    let temp = tempfile::tempdir().expect("temp workspace");
    let root = temp.path();
    let path = root.join("main.roc");
    let text = "app [main!] {\n\troc: \"nightly\",\n}\n\nmain! = |_| {\n\tvar $count = 0\n\t$count = $count + 1\n\t$count\n}\n";
    std::fs::write(&path, text).expect("main");
    let uri = uri_for(&path);
    let host = AnalysisHost::new(root.to_path_buf(), AnalysisConfig::default());
    let analysis = host.analyze(&uri).expect("analysis");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &analysis, &config);
    let offset = offset_of(text, "var $count") + "var ".len() as u32;
    assert!(
        navigation::rename(
            &ctx,
            &rename_params(&uri, position(&analysis.source, offset), "count")
        )
        .is_none(),
        "rename must not turn a variable into a constant name"
    );
    let edit = navigation::rename(
        &ctx,
        &rename_params(&uri, position(&analysis.source, offset), "$total"),
    )
    .expect("variable rename");
    let edits = workspace_edit_edits(&edit);
    assert_eq!(edits.len(), 4);
    assert!(edits.iter().all(|(_, edit)| edit.new_text == "$total"));
}

#[test]
fn goto_resolves_root_relative_imports_and_document_links() {
    let temp = tempfile::tempdir().expect("temp workspace");
    let root = temp.path();
    std::fs::create_dir_all(root.join("Internal")).expect("dir");
    let parser_path = root.join("Internal/Parser.roc");
    std::fs::write(&parser_path, "module [parse]\n\nparse = |n| n\n").expect("parser");
    let path = root.join("main.roc");
    let text = "app [main!] {\n\troc: \"nightly\",\n}\n\nimport /Internal/Parser\n\nmain! = |_| Parser.parse(1)\n";
    std::fs::write(&path, text).expect("main");
    let uri = uri_for(&path);
    let host = AnalysisHost::new(root.to_path_buf(), AnalysisConfig::default());
    let analysis = host.analyze(&uri).expect("analysis");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &analysis, &config);
    let offset = offset_of(text, "parse(1)");
    let locations = goto_locations(navigation::goto(
        &ctx,
        &goto_params(&uri, position(&analysis.source, offset)),
        NavigationKind::Definition,
    ));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, uri_for(&parser_path));
}

#[test]
fn rename_works_for_type_tags_and_record_fields() {
    let temp = tempfile::tempdir().expect("temp workspace");
    let root = temp.path();
    let path = root.join("main.roc");
    let text = "Color :: [Red, Green]\nPerson :: { name : Str }\n\ncolor = Color.Red\nperson = { name: \"Ada\" }\n";
    std::fs::write(&path, text).expect("main");
    let uri = uri_for(&path);
    let host = AnalysisHost::new(root.to_path_buf(), AnalysisConfig::default());
    let analysis = host.analyze(&uri).expect("analysis");
    let config = Config::default();
    let ctx = FeatureContext::new(&host, &analysis, &config);
    let red = offset_of(text, "Red");
    let edit = navigation::rename(
        &ctx,
        &rename_params(&uri, position(&analysis.source, red), "Crimson"),
    )
    .expect("tag rename");
    assert_eq!(workspace_edit_edits(&edit).len(), 2);
    let name = offset_of(text, "name : Str");
    let edit = navigation::rename(
        &ctx,
        &rename_params(&uri, position(&analysis.source, name), "title"),
    )
    .expect("field rename");
    assert_eq!(workspace_edit_edits(&edit).len(), 2);
}

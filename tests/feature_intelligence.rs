//! Feature tests for completion, hover, signature help, and inlay hints.

use std::path::PathBuf;
use std::sync::Arc;

use lsp_types::{
    CompletionContext, CompletionParams, CompletionResponse, CompletionTriggerKind, HoverParams,
    InlayHintLabel, InlayHintParams, PartialResultParams, Position, SignatureHelpParams,
    TextDocumentIdentifier, TextDocumentPositionParams, Url, WorkDoneProgressParams,
};

use hardroc::analysis::{AnalysisConfig, AnalysisHost, AnalyzedFile};
use hardroc::config::Config;
use hardroc::features::intelligence;
use hardroc::features::FeatureContext;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("workspace")
}

struct Fixture {
    host: AnalysisHost,
    analysis: Arc<AnalyzedFile>,
    config: Config,
    uri: Url,
    text: String,
}

fn fixture(name: &str) -> Fixture {
    let root = workspace_root().canonicalize().expect("fixture workspace");
    let path = root.join(name);
    let uri = Url::from_file_path(&path).expect("absolute fixture path");
    let host = AnalysisHost::new(root, AnalysisConfig::default());
    let analysis = host.analyze(&uri).expect("fixture is analyzed");
    let text = analysis.source.text().to_string();
    Fixture {
        host,
        analysis,
        config: Config::default(),
        uri,
        text,
    }
}

impl Fixture {
    fn position(&self, offset: u32) -> Position {
        self.analysis.source.position_at(offset)
    }

    fn offset_after(&self, needle: &str, extra: usize) -> u32 {
        (self
            .text
            .find(needle)
            .unwrap_or_else(|| panic!("missing {needle:?}"))
            + needle.len()
            + extra) as u32
    }

    fn offset_of(&self, needle: &str) -> u32 {
        self.text
            .find(needle)
            .unwrap_or_else(|| panic!("missing {needle:?}")) as u32
    }
}

fn completion(fixture: &Fixture, offset: u32) -> Vec<lsp_types::CompletionItem> {
    let ctx = FeatureContext::new(&fixture.host, &fixture.analysis, &fixture.config);
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: fixture.uri.clone(),
            },
            position: fixture.position(offset),
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: Some(CompletionContext {
            trigger_kind: CompletionTriggerKind::INVOKED,
            trigger_character: None,
        }),
    };
    match intelligence::completion(&ctx, &params) {
        Some(CompletionResponse::Array(items)) => items,
        Some(CompletionResponse::List(list)) => list.items,
        None => Vec::new(),
    }
}

fn hover(fixture: &Fixture, offset: u32) -> Option<lsp_types::Hover> {
    let ctx = FeatureContext::new(&fixture.host, &fixture.analysis, &fixture.config);
    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: fixture.uri.clone(),
            },
            position: fixture.position(offset),
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    intelligence::hover(&ctx, &params)
}

#[test]
fn completion_offers_module_members_and_record_fields() {
    let fixture = fixture("main.roc");
    let items = completion(&fixture, fixture.offset_after("Color.", 0));
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"base"),
        "Color members missing: {labels:?}"
    );
    assert!(
        labels.contains(&"to_hex"),
        "Color members missing: {labels:?}"
    );

    let items = completion(&fixture, fixture.offset_after("settings.", 0));
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"greet"),
        "record fields missing: {labels:?}"
    );
    assert!(
        labels.contains(&"label"),
        "record fields missing: {labels:?}"
    );
}

#[test]
fn completion_uses_receiver_type_for_arbitrary_chains() {
    let fixture = fixture("chains.roc");

    let items = completion(&fixture, fixture.offset_after("Dict.empty().in", 0));
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    for expected in ["insert", "is_empty", "get"] {
        assert!(
            labels.contains(&expected),
            "Dict member {expected} missing after call-chain dot: {labels:?}"
        );
    }

    let items = completion(&fixture, fixture.offset_after("Widget.make(\"demo\").", 0));
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"describe"),
        "Widget method missing after call-chain dot: {labels:?}"
    );

    let items = completion(
        &fixture,
        fixture.offset_after("Widget.make(\"demo\").describe().", 0),
    );
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"concat"),
        "Str members missing after chained method-call dot: {labels:?}"
    );

    let items = completion(&fixture, fixture.offset_after("Dict.empty()\n\t\t.in", 0));
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"insert"),
        "Dict members missing after multiline chain dot: {labels:?}"
    );

    let items = completion(
        &fixture,
        fixture.offset_after("Widget.make(\"demo\").label.", 0),
    );
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"concat"),
        "Str members missing after field-chain dot: {labels:?}"
    );

    let items = completion(
        &fixture,
        fixture.offset_after(".insert((11, 20), \"A\").in", 0),
    );
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"insert"),
        "Dict members missing after multiline insert chain: {labels:?}"
    );

    let items = completion(&fixture, fixture.offset_after("\t\t$map = $map.in", 0));
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"insert"),
        "Dict members missing after mutable-variable dot: {labels:?}"
    );
}

#[test]
fn completion_offers_structural_alias_fields_in_record_literals() {
    let structural = fixture("records.roc");

    for needle in ["top_pair = {", "pair : Pair(U64, Str)\n\tpair = {"] {
        let items = completion(&structural, structural.offset_after(needle, 0));
        let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
        assert!(
            labels.contains(&"first") && labels.contains(&"second"),
            "Pair fields missing after {needle:?}: {labels:?}"
        );
    }

    let widget = fixture("Src/Widget.roc");
    let items = completion(&widget, widget.offset_after("Widget.({", 0));
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels == ["label"],
        "nominal constructor should offer only its fields: {labels:?}"
    );
}

#[test]
fn completion_offers_import_targets_after_import_keyword() {
    let fixture = fixture("main.roc");
    let items = completion(&fixture, fixture.offset_after("import ", 0));
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"Color") && labels.contains(&"Src/Widget"),
        "import completion missing workspace modules: {labels:?}"
    );

    let items = completion(&fixture, fixture.offset_after("import util.", 0));
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"Util"),
        "package import completion missing Util: {labels:?}"
    );
}

#[test]
fn completion_offers_exposing_items_for_a_package_import() {
    let fixture = fixture("exposing.roc");
    let items = completion(
        &fixture,
        fixture.offset_after("import util.Util exposing ", 0),
    );
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"greet"),
        "exposing completion missing exported item: {labels:?}"
    );
}

#[test]
fn completion_offers_primitive_numeric_types_in_annotations() {
    let fixture = fixture("types.roc");

    let items = completion(&fixture, fixture.offset_after("count : ", 0));
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    for expected in ["I64", "U64", "F64", "Dec"] {
        assert!(
            labels.contains(&expected),
            "numeric primitive {expected} missing from type completion: {labels:?}"
        );
    }

    let items = completion(&fixture, fixture.offset_after("count : I", 0));
    let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"I64") && labels.contains(&"I32"),
        "numeric primitives missing while filtering a type prefix: {labels:?}"
    );
}

#[test]
fn hover_shows_imported_signature_and_documentation() {
    let fixture = fixture("main.roc");
    let offset = fixture.offset_of("Util.greet") + "Util.".len() as u32;
    let value = serde_json::to_value(hover(&fixture, offset).expect("hover for Util.greet"))
        .expect("hover serializes");
    let text = value.to_string();
    assert!(text.contains("greet"), "hover lacks name: {text}");
    assert!(
        text.contains("Greet someone"),
        "hover lacks module documentation: {text}"
    );
}

#[test]
fn signature_help_works_at_the_start_of_a_string_argument() {
    let fixture = fixture("main.roc");
    let offset = fixture.offset_after("Widget.make(", 0);
    let ctx = FeatureContext::new(&fixture.host, &fixture.analysis, &fixture.config);
    let params = SignatureHelpParams {
        context: None,
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: fixture.uri.clone(),
            },
            position: fixture.position(offset),
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    let help = intelligence::signature_help(&ctx, &params).expect("signature help");
    let signature = &help.signatures[0];
    assert!(
        signature.label.contains("make(arg1: Str)"),
        "unexpected signature: {}",
        signature.label
    );
    assert!(signature.documentation.is_some());
}

#[test]
fn inlay_hints_show_parameter_and_return_types() {
    let fixture = fixture("Color.roc");
    let ctx = FeatureContext::new(&fixture.host, &fixture.analysis, &fixture.config);
    let params = InlayHintParams {
        work_done_progress_params: WorkDoneProgressParams::default(),
        text_document: TextDocumentIdentifier {
            uri: fixture.uri.clone(),
        },
        range: fixture.analysis.source.range(hardroc::text::TextRange::new(
            0,
            fixture.analysis.source.len(),
        )),
    };
    let hints = intelligence::inlay_hints(&ctx, &params).expect("inlay hints");
    let labels: Vec<String> = hints
        .iter()
        .map(|hint| match &hint.label {
            InlayHintLabel::String(label) => label.clone(),
            InlayHintLabel::LabelParts(parts) => parts
                .iter()
                .map(|part| part.value.clone())
                .collect::<Vec<_>>()
                .join(""),
        })
        .collect();
    assert!(
        labels.iter().any(|label| label.contains(": Color")),
        "missing parameter type hint: {labels:?}"
    );
    assert!(
        labels.iter().any(|label| label.contains(": Str")),
        "missing return type hint: {labels:?}"
    );
}

#[test]
fn completion_resolve_fills_builtin_documentation() {
    let fixture = fixture("main.roc");
    let items = completion(&fixture, fixture.offset_after("Color.", 0));
    let item = items
        .into_iter()
        .find(|item| item.label == "to_hex")
        .expect("to_hex completion");
    let ctx = FeatureContext::new(&fixture.host, &fixture.analysis, &fixture.config);
    let resolved = intelligence::resolve_completion(&ctx, item);
    assert!(
        resolved.detail.is_some() || resolved.documentation.is_some(),
        "completion resolve produced no detail or documentation"
    );
}

#[test]
fn package_import_completion_inserts_the_module_without_duplicating_the_alias() {
    let temp = tempfile::tempdir().expect("temp workspace");
    let root = temp.path();
    std::fs::create_dir_all(root.join("lib/Sub")).expect("package dirs");
    std::fs::write(
        root.join("lib/main.roc"),
        "package [Widget, Mod] {}\n\nimport Sub/Mod as Mod\n",
    )
    .expect("package root");
    std::fs::write(root.join("lib/Widget.roc"), "module [make]\n\nmake = 1\n").expect("widget");
    std::fs::write(
        root.join("lib/Sub/Mod.roc"),
        "module [value]\n\nvalue = 1\n",
    )
    .expect("nested module");

    let host = AnalysisHost::new(
        root.to_path_buf(),
        AnalysisConfig {
            inspect_packages: false,
            ..AnalysisConfig::default()
        },
    );
    for (name, import) in [
        ("Root.roc", "import lib.\n\nmain! = |_| {}\n"),
        ("Partial.roc", "import lib.Wi\n\nmain! = |_| {}\n"),
        ("Nested.roc", "import lib.\n\nmain! = |_| {}\n"),
    ] {
        let path = root.join(name);
        std::fs::write(
            &path,
            format!(
                "app [main!] {{\n\tlib: \"./lib/main.roc\",\n\troc: \"nightly\",\n}}\n\n{import}"
            ),
        )
        .expect("app");
        host.rebuild_workspace();
        let uri = Url::from_file_path(&path).expect("app uri");
        let analysis = host.analyze(&uri).expect("app analysis");
        let text = analysis.source.text().to_string();
        let offset = text.find(import).expect("import offset") as u32
            + import.split('\n').next().unwrap_or("").len() as u32;
        let config = Config::default();
        let ctx = FeatureContext::new(&host, &analysis, &config);
        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: analysis.source.position_at(offset),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: None,
        };
        let items = match intelligence::completion(&ctx, &params) {
            Some(CompletionResponse::Array(items)) => items,
            Some(CompletionResponse::List(list)) => list.items,
            None => Vec::new(),
        };
        let expected = if name == "Nested.roc" {
            "Mod"
        } else {
            "Widget"
        };
        let widget = items
            .iter()
            .find(|item| item.label == expected)
            .unwrap_or_else(|| {
                panic!(
                    "missing package completion in {name}: {:?}",
                    items.iter().map(|item| &item.label).collect::<Vec<_>>()
                )
            });
        assert_eq!(widget.label, expected);
        let inserted = match widget.text_edit.as_ref() {
            Some(lsp_types::CompletionTextEdit::Edit(edit)) => edit.new_text.clone(),
            _ => widget.insert_text.clone().unwrap_or_default(),
        };
        assert!(
            !inserted.contains("lib.lib"),
            "package alias duplicated in completion: {inserted}"
        );
        assert_eq!(inserted, widget.label);
        assert!(
            !items.iter().any(|item| item.label == "Sub.Mod"),
            "private package modules must not be offered: {:?}",
            items.iter().map(|item| &item.label).collect::<Vec<_>>()
        );
    }

    let private_path = root.join("Private.roc");
    std::fs::write(
        &private_path,
        "app [main!] {\n\tlib: \"./lib/main.roc\",\n\troc: \"nightly\",\n}\n\nimport lib.Sub.Mod\n\nmain! = |_| {}\n",
    )
    .expect("private importer");
    host.rebuild_workspace();
    let private_uri = Url::from_file_path(&private_path).expect("private uri");
    let private_analysis = host.analyze(&private_uri).expect("private analysis");
    assert!(private_analysis.imports[0].target.is_none());
    assert!(
        private_analysis
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "unresolved-import"),
        "{:?}",
        private_analysis.diagnostics
    );
}

#[test]
fn inlay_hints_respect_parameter_types_setting() {
    let mut fixture = fixture("Color.roc");
    fixture.config.inlay_hints.parameter_types = false;
    let ctx = FeatureContext::new(&fixture.host, &fixture.analysis, &fixture.config);
    let params = InlayHintParams {
        work_done_progress_params: WorkDoneProgressParams::default(),
        text_document: TextDocumentIdentifier {
            uri: fixture.uri.clone(),
        },
        range: fixture.analysis.source.range(hardroc::text::TextRange::new(
            0,
            fixture.analysis.source.len(),
        )),
    };
    let hints = intelligence::inlay_hints(&ctx, &params).expect("inlay hints");
    let tooltips: Vec<String> = hints
        .iter()
        .filter_map(|hint| hint.tooltip.as_ref())
        .map(|tooltip| match tooltip {
            lsp_types::InlayHintTooltip::String(text) => text.clone(),
            lsp_types::InlayHintTooltip::MarkupContent(markup) => markup.value.clone(),
        })
        .collect();
    assert!(
        !tooltips
            .iter()
            .any(|tooltip| tooltip.contains("type of parameter")),
        "parameter type hints were disabled: {tooltips:?}"
    );
}

#[test]
fn completion_resolve_documentation_setting_defers_docs_to_resolve() {
    let mut fixture = fixture("main.roc");
    fixture.config.completion.resolve_documentation = false;
    let items = completion(&fixture, fixture.offset_after("Color.", 0));
    let item = items
        .into_iter()
        .find(|item| item.label == "to_hex")
        .expect("to_hex completion");
    assert!(
        item.documentation.is_none(),
        "documentation was sent eagerly despite resolveDocumentation=false"
    );
    let ctx = FeatureContext::new(&fixture.host, &fixture.analysis, &fixture.config);
    let resolved = intelligence::resolve_completion(&ctx, item);
    assert!(
        resolved.documentation.is_some() || resolved.detail.is_some(),
        "completion resolve did not supply deferred documentation"
    );
}

#[test]
fn nested_type_imports_resolve_to_the_module_and_export() {
    let temp = tempfile::tempdir().expect("temp workspace");
    let root = temp.path();
    std::fs::write(
        root.join("Parser.roc"),
        "module [ParseErr]\n\nParseErr : [ParseErr Str]\n",
    )
    .expect("parser module");
    let path = root.join("NestedImport.roc");
    std::fs::write(
        &path,
        "app [main!] {\n\troc: \"nightly\",\n}\n\nimport Parser.ParseErr\n\nx : ParseErr\nx = ParseErr(\"bad\")\n\nmain! = |_| x\n",
    )
    .expect("app");
    let host = AnalysisHost::new(
        root.to_path_buf(),
        AnalysisConfig {
            inspect_packages: false,
            ..AnalysisConfig::default()
        },
    );
    let uri = Url::from_file_path(&path).expect("uri");
    let analysis = host.analyze(&uri).expect("analysis");
    assert_eq!(analysis.imports.len(), 1);
    assert_eq!(analysis.imports[0].nested, vec!["ParseErr"]);
    assert_eq!(
        analysis.imports[0]
            .target
            .as_ref()
            .map(|target| target.name.as_str()),
        Some("Parser")
    );
    assert!(
        !analysis
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "unresolved-import"),
        "{:?}",
        analysis.diagnostics
    );
    assert!(
        !analysis
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "unused-import"),
        "{:?}",
        analysis.diagnostics
    );
}

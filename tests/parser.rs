//! Parser regressions for module headers and malformed input.

use hardroc::syntax::ast::{HeaderKind, ItemKind};
use hardroc::syntax::parse;

#[test]
fn deprecated_module_header_is_parsed_without_syntax_errors() {
    let source = "module [main, Color]\n\nmain = 42\n";
    let parsed = parse(source);
    assert!(
        parsed.diagnostics.is_empty(),
        "module header should not produce diagnostics: {:?}",
        parsed.diagnostics
    );
    let header = parsed.module.header.as_ref().expect("module header");
    assert_eq!(header.kind, HeaderKind::Module);
    let exposed: Vec<&str> = header
        .provides
        .iter()
        .map(|name| name.text.as_str())
        .collect();
    assert_eq!(exposed, ["main", "Color"]);
    assert!(parsed
        .module
        .items
        .iter()
        .any(|item| matches!(&item.kind, ItemKind::Definition(definition) if definition.pattern.range.start > header.range.end)));
}

#[test]
fn module_header_without_trailing_newline_still_parses() {
    let parsed = parse("module [main]\n\nmain = 1\n");
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    assert_eq!(
        parsed.module.header.as_ref().map(|header| header.kind),
        Some(HeaderKind::Module)
    );
}

#[test]
fn malformed_input_returns_partial_tree_and_diagnostics() {
    let parsed = parse("main =\n");
    assert!(!parsed.diagnostics.is_empty());
    assert!(parsed
        .module
        .items
        .iter()
        .any(|item| matches!(item.kind, ItemKind::Definition(_))));
}

#[test]
fn parses_type_bodies_signatures_padding_and_nominal_patterns() {
    let source = r#"module [f]

Platform :: [].{
    parse : Spec(a, b),
    {
        tag : Str,
        start_payloads : b, U64 -> Try(b, err),
        missing : err,
    } -> Try(a, err)

    parse = |state| state

    State :: [Input(Str)]
    Slot(v) := {
        z : U32,
        _ : U32,
        value : v,
    }.{
        new : U32, v -> Slot(v)
        new = |z, value| { z, value }
    }
}

f = |set|
    match set {
        Set.(HashMap(data)) => data
    }
"#;
    let parsed = parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let platform = parsed
        .module
        .items
        .iter()
        .find_map(|item| match &item.kind {
            ItemKind::TypeDecl(decl) if decl.name.text == "Platform" => Some(decl),
            _ => None,
        })
        .expect("Platform type declaration");
    assert!(
        platform.members.iter().any(|item| matches!(
            &item.kind,
            ItemKind::Definition(definition) if definition.annotation.is_some()
        )),
        "multiline member signature should merge into its definition"
    );
}

#[test]
fn parses_effectful_loops_method_chains_and_multiline_conditions() {
    let source = r#"module [f]

hooks = { run: |x| x }

f = |set, name|
    for! item in set {
        if (hooks.run)(name) == 1 and item > 0 {
            name.raw
                ->replace_all("!", "")
                ->lower()
        } else {
            ""
        }
    }
"#;
    let parsed = parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
}

#[test]
fn parses_platform_requires_signature_and_nested_string_expressions() {
    let source = r#"platform ""
    requires {} { main! : () => Str }
    exposes [Stdout]
    packages {}
    provides { "roc_main": main! }
    hosted { "stdout_line": Stdout.line! }
    targets: { wasm32: { inputs: ["host.wasm", app] } }

import Stdout

message = "${Foo.fizz(fuzz.beta, "buzz")}"
"#;
    let parsed = parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let header = parsed.module.header.as_ref().expect("platform header");
    assert_eq!(header.kind, HeaderKind::Platform);
    assert!(header.requires.is_some());
    assert_eq!(header.exposes.len(), 1);
}

#[test]
fn when_and_is_remain_ordinary_identifiers() {
    let parsed = parse("module [when]\n\nwhen = |is| is\n\nvalue = when(1)\n");
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
}

#[test]
fn var_annotations_and_uninitialized_declarations_parse() {
    let source = "module [count]\n\n$count : U64\nvar $count\n\ncount = $count\n";
    let parsed = parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let var = parsed
        .module
        .items
        .iter()
        .find_map(|item| match &item.kind {
            ItemKind::Definition(definition) if definition.is_var => Some(definition),
            _ => None,
        })
        .expect("var definition");
    assert!(var.annotation.is_some(), "{var:#?}");
}

#[test]
fn continue_is_an_ordinary_identifier() {
    let source = "module [continue]\n\ncontinue = |continue| continue\n";
    let parsed = parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let definition = parsed
        .module
        .items
        .iter()
        .find_map(|item| match &item.kind {
            ItemKind::Definition(definition) => Some(definition),
            _ => None,
        })
        .expect("definition");
    assert_eq!(
        definition.simple_name().map(|name| name.text.as_str()),
        Some("continue")
    );
}

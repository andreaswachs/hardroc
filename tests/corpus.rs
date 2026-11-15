//! Optional corpus regression: set `ROC_EXAMPLES_DIR` to a Roc examples
//! checkout (or any directory tree of `.roc` files) and the parser must accept
//! every file without syntax diagnostics.

use std::path::Path;

use hardroc::syntax::parse;

#[test]
fn parses_every_roc_file_in_roc_examples_dir() {
    let Some(root) = std::env::var_os("ROC_EXAMPLES_DIR") else {
        return;
    };
    let root = Path::new(&root);
    let mut files = Vec::new();
    collect(root, &mut files);
    assert!(
        !files.is_empty(),
        "ROC_EXAMPLES_DIR={} contained no .roc files",
        root.display()
    );
    let mut failures = Vec::new();
    for path in &files {
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let parsed = parse(&source);
        if !parsed.diagnostics.is_empty() {
            failures.push(format!("{}: {:?}", path.display(), parsed.diagnostics));
        }
    }
    assert!(
        failures.is_empty(),
        "parser rejected {} of {} corpus files:\n{}",
        failures.len(),
        files.len(),
        failures.join("\n")
    );
}

fn collect(path: &Path, files: &mut Vec<std::path::PathBuf>) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.is_file() {
        if path.extension().is_some_and(|extension| extension == "roc") {
            files.push(path.to_path_buf());
        }
        return;
    }
    if !metadata.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    let mut children: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    children.sort();
    for child in children {
        if child
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.') && name != ".roc_echo_platform")
        {
            continue;
        }
        collect(&child, files);
    }
}

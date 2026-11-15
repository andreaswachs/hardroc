//! Development tool: parse a Roc file or a directory tree and report syntax
//! diagnostics. Not part of the LSP.
//!
//! Usage:
//!   cargo run --example parse_corpus -- path/to/file.roc [more...]
//!   cargo run --example parse_corpus -- --all path/to/examples
//!
//! Exit status is non-zero when any file produced diagnostics.

use std::path::{Path, PathBuf};

use hardroc::syntax::parse;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut all = false;
    let mut paths = Vec::new();
    for arg in args.by_ref() {
        if arg == "--all" {
            all = true;
        } else {
            paths.push(PathBuf::from(arg));
        }
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }

    let mut files = Vec::new();
    for path in paths {
        collect(&path, all, &mut files);
    }
    files.sort();
    files.dedup();

    let mut failures = 0usize;
    let mut total_diagnostics = 0usize;
    for path in &files {
        let Ok(source) = std::fs::read_to_string(path) else {
            eprintln!("skip {}: unreadable", path.display());
            continue;
        };
        let parsed = parse(&source);
        if parsed.diagnostics.is_empty() {
            if std::env::var_os("HARDROC_VERBOSE").is_some() {
                println!("ok {} ({} tokens)", path.display(), parsed.tokens.len());
            }
        } else {
            failures += 1;
            total_diagnostics += parsed.diagnostics.len();
            println!(
                "{}: {} diagnostic(s)",
                path.display(),
                parsed.diagnostics.len()
            );
            for diagnostic in parsed.diagnostics.iter().take(20) {
                let line = source[..diagnostic.range.start as usize]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count()
                    + 1;
                println!(
                    "  {}:{} {} [{}]",
                    path.display(),
                    line,
                    diagnostic.message,
                    diagnostic.code
                );
            }
        }
    }
    println!(
        "parsed {} file(s): {} with diagnostics, {} diagnostic(s) total",
        files.len(),
        failures,
        total_diagnostics
    );
    if failures > 0 {
        std::process::exit(1);
    }
}

fn collect(path: &Path, all: bool, output: &mut Vec<PathBuf>) {
    if path.is_file() {
        output.push(path.to_path_buf());
        return;
    }
    if !path.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') && name != ".roc_echo_platform" {
            continue;
        }
        if path.is_dir() {
            if all {
                collect(&path, all, output);
            }
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("roc") {
            output.push(path);
        }
    }
}

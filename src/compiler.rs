//! `roc check` integration: background type checking and diagnostics parsing.

use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Range, Url};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct CompilerDiagnostic {
    pub uri: Url,
    pub range: Range,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub code: Option<String>,
}

/// Run `roc check` against a root file and parse its human-readable output.
/// ANSI colors are disabled.  Returns an empty vector on timeout or when the
/// compiler is unavailable; callers decide how to surface that.
pub async fn run_check(
    roc_path: &str,
    root_file: &Path,
    timeout_secs: u64,
) -> Result<Vec<CompilerDiagnostic>, String> {
    use tokio::process::Command;
    use tokio::time::{timeout, Duration};

    // `roc check` prints diagnostic paths relative to its working directory.
    // Run it from the root module's directory and resolve diagnostics against
    // that same directory, otherwise relative paths under `..` map incorrectly.
    let base = root_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let file_arg = root_file
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| root_file.to_path_buf());
    let mut command = Command::new(roc_path);
    command
        .current_dir(&base)
        .arg("check")
        .arg(&file_arg)
        .arg("--no-color")
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = timeout(Duration::from_secs(timeout_secs), command.output())
        .await
        .map_err(|_| format!("roc check timed out after {timeout_secs}s"))?
        .map_err(|error| format!("failed to run roc check: {error}"))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(parse_check_output(&text, &base))
}

/// Run `roc check` against a *shadow* of the workspace that contains the
/// current unsaved editor buffers.
///
/// `roc check` only reads files from disk.  To get live type diagnostics while
/// typing, mirror the workspace next to itself (so relative imports keep the
/// same depth), symlink the unchanged Roc files, and write the open buffers
/// over the mirrored copies.  Diagnostics are mapped back to the real paths
/// before being returned.
pub async fn run_check_with_overlay(
    roc_path: &str,
    root_file: &Path,
    timeout_secs: u64,
    workspace_root: &Path,
    overlays: &[(PathBuf, String)],
) -> Result<Vec<CompilerDiagnostic>, String> {
    let has_overlay = overlays.iter().any(|(path, _)| {
        path.starts_with(workspace_root) && path.extension().is_some_and(|ext| ext == "roc")
    });
    if !has_overlay {
        return run_check(roc_path, root_file, timeout_secs).await;
    }

    let shadow = match ShadowWorkspace::create(workspace_root, overlays) {
        Ok(shadow) => shadow,
        Err(shadow_error) => {
            // A failed overlay must not disable type checking altogether.
            return run_check(roc_path, root_file, timeout_secs)
                .await
                .map_err(|error| {
                    format!("{error} (shadow workspace unavailable: {shadow_error})")
                });
        }
    };
    let Some(relative_root) = root_file.strip_prefix(workspace_root).ok() else {
        return run_check(roc_path, root_file, timeout_secs).await;
    };
    let shadow_root_file = shadow.root.join(relative_root);
    let mut diagnostics = run_check(roc_path, &shadow_root_file, timeout_secs).await?;
    for diagnostic in &mut diagnostics {
        diagnostic.uri = shadow.remap_uri(&diagnostic.uri);
    }
    Ok(diagnostics)
}

struct ShadowWorkspace {
    root: PathBuf,
    canonical_root: PathBuf,
    original_root: PathBuf,
    canonical_original_root: PathBuf,
}

impl ShadowWorkspace {
    fn create(original_root: &Path, overlays: &[(PathBuf, String)]) -> std::io::Result<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        let parent = original_root.parent().unwrap_or_else(|| Path::new("."));
        let base_name = original_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace");
        let root = loop {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(
                ".hardroc-check-{}-{id}-{base_name}",
                std::process::id()
            ));
            if !candidate.exists() {
                fs::create_dir(&candidate)?;
                break candidate;
            }
        };

        let result = (|| {
            let overlay_map: HashMap<PathBuf, &str> = overlays
                .iter()
                .filter(|(path, _)| path.starts_with(original_root))
                .filter_map(|(path, text)| {
                    lexical_normalize(path)
                        .strip_prefix(lexical_normalize(original_root))
                        .ok()
                        .map(|relative| (relative.to_path_buf(), text.as_str()))
                })
                .collect();
            let mut written = HashMap::new();

            for entry in WalkDir::new(original_root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| {
                    if !entry.file_type().is_dir() {
                        return true;
                    }
                    !matches!(
                        entry.file_name().to_str(),
                        Some(".git" | "target" | "node_modules" | ".zig-cache" | "zig-cache")
                    )
                })
            {
                let entry = entry?;
                if !(entry.file_type().is_file() || entry.file_type().is_symlink()) {
                    continue;
                }
                if entry.path().extension().map_or(true, |ext| ext != "roc") {
                    continue;
                }
                let relative = entry
                    .path()
                    .strip_prefix(original_root)
                    .map_err(std::io::Error::other)?;
                let destination = root.join(relative);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                if let Some(text) = overlay_map.get(relative) {
                    fs::write(&destination, text)?;
                    written.insert(relative.to_path_buf(), ());
                } else {
                    link_or_copy(entry.path(), &destination)?;
                }
            }

            // Open documents may be new unsaved files that are not present in
            // the on-disk walk yet.
            for (relative, text) in overlay_map {
                if written.contains_key(&relative) {
                    continue;
                }
                let destination = root.join(&relative);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(destination, text)?;
            }
            Ok::<(), std::io::Error>(())
        })();

        if let Err(error) = result {
            let _ = fs::remove_dir_all(&root);
            return Err(error);
        }

        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        let canonical_original_root = original_root
            .canonicalize()
            .unwrap_or_else(|_| original_root.to_path_buf());
        Ok(Self {
            root,
            canonical_root,
            original_root: original_root.to_path_buf(),
            canonical_original_root,
        })
    }

    fn remap_uri(&self, uri: &Url) -> Url {
        let Ok(path) = uri.to_file_path() else {
            return uri.clone();
        };
        let path = path
            .canonicalize()
            .unwrap_or_else(|_| lexical_normalize(&path));
        let mapped = if let Ok(relative) = path.strip_prefix(&self.canonical_root) {
            // Keep the path spelling of the original workspace: on macOS
            // `/var` canonicalizes to `/private/var`, and publishing under the
            // latter would not match the URI the client opened.
            self.original_root.join(relative)
        } else if let Ok(relative) = path.strip_prefix(&self.canonical_original_root) {
            self.original_root.join(relative)
        } else {
            path
        };
        Url::from_file_path(&mapped).unwrap_or_else(|_| uri.clone())
    }
}

impl Drop for ShadowWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn link_or_copy(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, destination)
    }
    #[cfg(not(unix))]
    {
        fs::copy(source, destination).map(|_| ())
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => output.push(prefix.as_os_str()),
            Component::RootDir => output.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            Component::Normal(part) => output.push(part),
        }
    }
    output
}

/// Parse the compiler's diagnostic blocks.  The stable part of the format is
/// the header line containing `file:line:column`; the message and caret span
/// follow.  We deliberately treat parsing as best-effort and never let an
/// unrecognized block discard the rest.
pub fn parse_check_output(text: &str, base: &Path) -> Vec<CompilerDiagnostic> {
    let lines: Vec<&str> = text.lines().collect();
    let mut output = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if let Some((severity, path, line_no, column)) = parse_header(line) {
            let mut body = Vec::new();
            let mut range = Range {
                start: lsp_types::Position::new(
                    line_no.saturating_sub(1),
                    column.saturating_sub(1),
                ),
                end: lsp_types::Position::new(line_no.saturating_sub(1), column),
            };
            let mut cursor = index + 1;
            let mut caret_found = false;
            while cursor < lines.len() {
                let candidate = lines[cursor];
                if parse_header(candidate).is_some() || candidate.trim_start().starts_with("── ")
                {
                    break;
                }
                if !caret_found {
                    if let Some((start, len)) = caret_span(candidate) {
                        range.start.character = start;
                        range.end.character = start.saturating_add(len);
                        caret_found = true;
                        cursor += 1;
                        continue;
                    }
                }
                body.push(candidate);
                cursor += 1;
            }
            let message = clean_body(&body);
            if let Some(uri) = resolve_uri(&path, base) {
                output.push(CompilerDiagnostic {
                    uri,
                    range,
                    severity,
                    message,
                    code: None,
                });
            }
            index = cursor;
        } else {
            index += 1;
        }
    }
    output
}

fn parse_header(line: &str) -> Option<(DiagnosticSeverity, String, u32, u32)> {
    if !line.starts_with("── ") {
        return None;
    }
    // Header shape: `── ✗ type mismatch ─────── main.roc:3:9`
    let trimmed = line.trim_end_matches('─').trim();
    let location = trimmed.rsplit_once(' ')?.1;
    let (path_and_line, column_text) = location.rsplit_once(':')?;
    let (path, line_text) = path_and_line.rsplit_once(':')?;
    let line_no = line_text.parse::<u32>().ok()?;
    let column = column_text.parse::<u32>().ok()?;
    let lowercase = line.to_ascii_lowercase();
    let severity = if line.contains('✗')
        || line.contains('✘')
        || lowercase.contains(" error ")
        || lowercase.contains("── error")
    {
        DiagnosticSeverity::ERROR
    } else if line.contains('●')
        || line.contains('⚠')
        || line.contains('✋')
        || lowercase.contains(" warning ")
        || lowercase.contains("── warning")
    {
        DiagnosticSeverity::WARNING
    } else {
        return None;
    };
    Some((severity, path.to_string(), line_no, column))
}

fn caret_span(line: &str) -> Option<(u32, u32)> {
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    let start = trimmed.len() - trimmed.trim_start().len();
    let rest = trimmed[start..].trim_start_matches(['^', '~']);
    if rest.len() == trimmed[start..].len() {
        return None;
    }
    let caret_len = trimmed[start..].len() - rest.len();
    Some((start as u32, caret_len as u32))
}

fn resolve_uri(path: &str, base: &Path) -> Option<Url> {
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        crate::analysis::packages::normalize_path(&base.join(path))
    };
    Url::from_file_path(absolute).ok()
}

fn clean_body(lines: &[&str]) -> String {
    let mut output = String::new();
    let mut previous_blank = false;
    for line in lines {
        let line = line.trim_end();
        if line.is_empty() {
            if !previous_blank && !output.is_empty() {
                output.push('\n');
            }
            previous_blank = true;
            continue;
        }
        previous_blank = false;
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(line);
        output.push('\n');
    }
    output.trim().to_string()
}

pub fn to_lsp(diagnostic: &CompilerDiagnostic) -> Diagnostic {
    Diagnostic {
        range: diagnostic.range,
        severity: Some(diagnostic.severity),
        code: diagnostic.code.clone().map(NumberOrString::String),
        code_description: None,
        source: Some("roc".to_string()),
        message: diagnostic.message.clone(),
        related_information: None,
        tags: None,
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_error_blocks() {
        let text = concat!(
            "── ✗ type mismatch ─────────────────────────── main.roc:3:9\n",
            "\n",
            "This number is being used where a non-number type is needed.\n",
            "\n",
            "x = 42\n",
            "    ^^\n",
            "\n",
            "── 1 error and 0 warnings ─── main.roc\n",
        );
        let diagnostics = parse_check_output(text, Path::new("/tmp/project"));
        assert_eq!(diagnostics.len(), 1);
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic.range.start.line, 2);
        assert_eq!(diagnostic.range.start.character, 4);
        assert_eq!(diagnostic.range.end.character, 6);
        assert!(diagnostic.message.contains("non-number"));
    }
    #[test]
    fn parses_real_compiler_output() {
        let text = concat!(
            "── ✗ type mismatch ──────────────────────────────────────────────── main.roc:3:9\n",
            "\n",
            "This number is being used where a non-number type is needed.\n",
            "\n",
            "x = 42\n",
            "    ^^\n",
            "\n",
            "Other code expects this to have the type:\n",
            "\n",
            "    Str\n",
            "\n",
            "Hint: Write the number in quotes to make it a Str, or convert it with .to_str().\n",
            "\n",
            "── ✗ type mismatch ──────────────────────────────────────────────── main.roc:5:5\n",
            "\n",
            "The platform requires  main!  to return a specific type.\n",
            "\n",
            "echo!(x)\n",
            "^^^^^^^^\n",
            "\n",
            "── 2 errors and 0 warnings ──────────────────────────────────────────── main.roc\n",
            "── ● duplicate definition ───────────────────────────────────────── main.roc:3:5\n",
            "\n",
            "The name x is being redeclared here.\n",
            "\n",
            "x = 2\n",
            "^\n",
            "\n",
            "── 0 errors and 1 warning ───────────────────────────────────────────── main.roc\n",
        );
        let diagnostics = parse_check_output(text, Path::new("/tmp/project"));
        assert_eq!(diagnostics.len(), 3);
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::ERROR);
        assert_eq!(diagnostics[0].range.start.character, 4);
        assert_eq!(diagnostics[1].range.start.character, 0);
        assert_eq!(diagnostics[1].range.end.character, 8);
        assert_eq!(diagnostics[2].severity, DiagnosticSeverity::WARNING);
    }
}

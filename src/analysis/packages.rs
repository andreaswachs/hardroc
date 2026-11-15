//! Roc package discovery and inspection.
//!
//! Roc's package sources live in the compiler cache (`$XDG_CACHE_HOME/roc` or
//! `~/.cache/roc` on macOS) under the bundle hash.  Path dependencies live
//! directly on disk.  The resolver never downloads anything: it uses sources
//! the compiler already fetched, and reports a helpful error when a dependency
//! has not been downloaded yet.

use std::path::{Path, PathBuf};

use lsp_types::Url;

use crate::syntax::ast::{DependencySource, HeaderKind, ModuleHeader};
use crate::text::TextRange;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PackageKind {
    App,
    Module,
    Package,
    Platform,
}

impl PackageKind {
    pub fn from_header(kind: HeaderKind) -> Self {
        match kind {
            HeaderKind::App => Self::App,
            HeaderKind::Module => Self::Module,
            HeaderKind::Package => Self::Package,
            HeaderKind::Platform => Self::Platform,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Module => "module",
            Self::Package => "package",
            Self::Platform => "platform",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DependencyInfo {
    pub shorthand: String,
    pub is_platform: bool,
    pub source: DependencySource,
    pub source_range: TextRange,
    /// Resolved local path or compiler-cache directory.
    pub root: Option<PathBuf>,
    /// Package metadata when the dependency root could be read.
    pub package: Option<Box<PackageInfo>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PackageInfo {
    pub shorthand: String,
    /// Additional import aliases that resolve to this same package root.
    #[serde(default)]
    pub aliases: Vec<String>,
    pub source: DependencySource,
    pub root: Option<PathBuf>,
    /// The exact root module file, when a dependency points at a nonstandard
    /// filename such as `Lib.roc` instead of `<root>/main.roc`.
    #[serde(default)]
    pub root_file: Option<PathBuf>,
    pub kind: PackageKind,
    /// Version parsed from a package URL, when present.
    pub version: Option<String>,
    /// Modules exposed by a package or platform header.
    pub exposes: Vec<String>,
    /// Modules provided by the package's own files (`package [A, B]` or all
    /// type modules found next to the root).
    pub modules: Vec<String>,
    pub dependencies: Vec<DependencyInfo>,
    pub header_range: Option<TextRange>,
    pub error: Option<String>,
}

impl PackageInfo {
    pub fn has_alias(&self, name: &str) -> bool {
        self.shorthand == name || self.aliases.iter().any(|alias| alias == name)
    }

    pub fn display_name(&self) -> &str {
        if self.shorthand.is_empty() {
            self.root
                .as_deref()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("package")
        } else {
            &self.shorthand
        }
    }

    pub fn source_display(&self) -> String {
        self.source.as_str().to_string()
    }

    pub fn has_source(&self) -> bool {
        self.root.as_ref().is_some_and(|root| root.is_dir())
    }
}

/// Roc's cache directory for downloaded package sources.
pub fn roc_cache_dir() -> Option<PathBuf> {
    if let Some(cache) = std::env::var_os("ROC_CACHE_DIR") {
        return Some(PathBuf::from(cache));
    }
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(cache).join("roc"));
    }
    // The compiler uses this location on every platform, including macOS.
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".cache").join("roc"))
}

pub fn package_cache_dir() -> Option<PathBuf> {
    roc_cache_dir().map(|cache| cache.join("packages"))
}

/// Extract the `<hash>` from a bundle URL.  The compiler names cache
/// directories after the base58 BLAKE3 hash in the filename.
pub fn bundle_hash(url: &str) -> Option<String> {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let file = without_query.rsplit('/').next()?;
    // Bundles can be `.tar.zst`, `.tar.br`, uncompressed tarballs, or a bare
    // hash. The compiler names its cache directory after the hash stem.
    let hash = file.split('.').next().unwrap_or(file);
    if hash.is_empty() {
        return None;
    }
    Some(hash.to_string())
}

pub fn parse_url_version(url: &str) -> Option<String> {
    for segment in url.split('/') {
        let mut parts = segment.split('.');
        let (Some(major), Some(minor), Some(patch), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if [major, minor, patch]
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
        {
            return Some(segment.to_string());
        }
    }
    None
}

/// Resolve a dependency source to a package root, without network access.
pub fn resolve_dependency_root(
    declaring_file: &Path,
    source: &DependencySource,
) -> (Option<PathBuf>, Option<String>) {
    match source {
        DependencySource::RocVersion(_) | DependencySource::CompilerOwned(_) => (None, None),
        DependencySource::Path(path) => {
            let base = declaring_file.parent().unwrap_or_else(|| Path::new("."));
            let joined = normalize_path(&base.join(path));
            // A path dependency usually points at the package's `main.roc`;
            // both file and directory forms are accepted.
            let root = if joined.is_dir() {
                joined
            } else if joined.is_file()
                || joined
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("roc"))
            {
                joined
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| joined.clone())
            } else {
                joined.clone()
            };
            if root.is_dir() {
                (Some(root), None)
            } else {
                (
                    Some(root.clone()),
                    Some(format!(
                        "dependency path does not exist: {}",
                        root.display()
                    )),
                )
            }
        }
        DependencySource::Url(url) => {
            let Some(hash) = bundle_hash(url) else {
                return (None, Some(format!("bundle URL has no hash: {url}")));
            };
            let Some(cache) = package_cache_dir() else {
                return (
                    None,
                    Some("could not locate Roc's package cache".to_string()),
                );
            };
            let root = cache.join(&hash);
            if root.is_dir() {
                (Some(root), None)
            } else {
                (
                    Some(root.clone()),
                    Some(format!(
                        "package is not downloaded yet; run `roc deps` to fetch it ({})",
                        root.display()
                    )),
                )
            }
        }
    }
}

/// Lexical path normalization (no filesystem access, no symlink resolution).
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !output.pop() {
                    output.push(component);
                }
            }
            other => output.push(other),
        }
    }
    output
}

pub fn uri_for_path(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

/// Best-effort package summary for a module header.  Source contents and
/// exposed module discovery are filled in by the workspace index.
pub fn package_from_header(
    header: &ModuleHeader,
    declaring_file: &Path,
    source: DependencySource,
    shorthand: &str,
) -> PackageInfo {
    let (root, error) = resolve_dependency_root(declaring_file, &source);
    let version = match &source {
        DependencySource::Url(url) => parse_url_version(url),
        DependencySource::CompilerOwned(_) => None,
        _ => None,
    };
    PackageInfo {
        shorthand: shorthand.to_string(),
        aliases: vec![shorthand.to_string()],
        source,
        root,
        root_file: None,
        kind: PackageKind::from_header(header.kind),
        version,
        exposes: header
            .exposes
            .iter()
            .map(|name| name.text.clone())
            .collect(),
        modules: header
            .provides
            .iter()
            .map(|name| name.text.clone())
            .collect(),
        dependencies: Vec::new(),
        header_range: Some(header.range),
        error,
    }
}

/// Locate the module file that owns a package or platform directory.
///
/// Downloaded bundles and path dependencies both use `main.roc`; a package
/// checked into the workspace may use any of the conventional names.
pub fn package_root_file(root: &Path) -> Option<PathBuf> {
    if root.is_file() {
        return Some(root.to_path_buf());
    }
    for name in ["main.roc", "app.roc", "package.roc"] {
        let candidate = root.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The exact `.roc` file a path dependency points at, if any.
pub fn resolve_dependency_file(
    declaring_file: &Path,
    source: &DependencySource,
) -> Option<PathBuf> {
    let DependencySource::Path(path) = source else {
        return None;
    };
    let base = declaring_file.parent().unwrap_or_else(|| Path::new("."));
    let joined = normalize_path(&base.join(path));
    (joined.is_file()
        && joined
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("roc")))
    .then_some(joined)
}

/// Read the compiler's package dependency sidecar for a cached package.
///
/// Current compilers write `<hash>.deps`: a line-oriented format (format 5)
/// with length-prefixed aliases/specs. Older trees may contain `.deps.json`;
/// both are accepted. The sidecar lives next to the extracted directory, not
/// inside it.
pub fn read_deps_json(root: &Path) -> Vec<(String, DependencySource, bool)> {
    if let Some(bytes) = read_sidecar_contents(root) {
        if let Some(deps) = parse_sidecar(&bytes) {
            return deps;
        }
        if let Ok(contents) = String::from_utf8(bytes) {
            let deps = parse_legacy_deps_json(&contents);
            if !deps.is_empty() {
                return deps;
            }
        }
    }
    read_deps_json_contents(root)
        .map(|contents| parse_legacy_deps_json(&contents))
        .unwrap_or_default()
}

fn parse_legacy_deps_json(contents: &str) -> Vec<(String, DependencySource, bool)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(contents) else {
        return Vec::new();
    };
    let Some(deps) = value.get("deps").and_then(|deps| deps.as_array()) else {
        return Vec::new();
    };
    let mut output: Vec<(String, DependencySource, bool)> = Vec::new();
    for dep in deps {
        let Some(alias) = dep.get("alias").and_then(|alias| alias.as_str()) else {
            continue;
        };
        let Some(spec) = dep.get("spec").and_then(|spec| spec.as_str()) else {
            continue;
        };
        if output.iter().any(|(existing, _, _)| existing == alias) {
            continue;
        }
        let source = if spec.starts_with("http://") || spec.starts_with("https://") {
            DependencySource::Url(spec.to_string())
        } else {
            DependencySource::Path(spec.to_string())
        };
        let is_platform = dep
            .get("is_platform")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        output.push((alias.to_string(), source, is_platform));
    }
    output
}

fn parse_sidecar(bytes: &[u8]) -> Option<Vec<(String, DependencySource, bool)>> {
    let mut cursor = 0usize;
    expect_literal(bytes, &mut cursor, b"roc-package-deps ")?;
    let format = parse_uint(bytes, &mut cursor)?;
    expect_literal(bytes, &mut cursor, b"\n")?;
    if format != 5 {
        return None;
    }
    for _ in 0..5 {
        let _ = parse_line(bytes, &mut cursor)?;
    }
    // The last fixed line is `deps <count>`; parse its count directly rather
    // than accepting any line.
    let deps_line_end = cursor - 1;
    let deps_line_start = bytes[..deps_line_end]
        .iter()
        .rposition(|byte| *byte == b'\n')?
        + 1;
    let deps_line = &bytes[deps_line_start..deps_line_end];
    let count_text = deps_line.strip_prefix(b"deps ")?;
    let count = std::str::from_utf8(count_text)
        .ok()?
        .parse::<usize>()
        .ok()?;

    let mut deps = Vec::with_capacity(count);
    for _ in 0..count {
        expect_literal(bytes, &mut cursor, b"dep ")?;
        let is_platform = match bytes.get(cursor)? {
            b'0' => false,
            b'1' => true,
            _ => return None,
        };
        cursor += 1;
        expect_literal(bytes, &mut cursor, b" ")?;
        let _owned = read_until(bytes, &mut cursor, b' ')?;
        let alias = length_prefixed(bytes, &mut cursor, b' ')?;
        let spec = length_prefixed(bytes, &mut cursor, b'\n')?;
        let alias = String::from_utf8_lossy(alias).to_string();
        let spec = String::from_utf8_lossy(spec).to_string();
        if alias.is_empty() || spec.is_empty() {
            continue;
        }
        let source = if spec.starts_with("http://") || spec.starts_with("https://") {
            DependencySource::Url(spec)
        } else {
            DependencySource::Path(spec)
        };
        if !deps.iter().any(|(existing, _, _)| existing == &alias) {
            deps.push((alias, source, is_platform));
        }
    }
    if cursor == bytes.len() {
        Some(deps)
    } else {
        None
    }
}

fn expect_literal(bytes: &[u8], cursor: &mut usize, literal: &[u8]) -> Option<()> {
    let end = cursor.checked_add(literal.len())?;
    if bytes.get(*cursor..end)? != literal {
        return None;
    }
    *cursor = end;
    Some(())
}

fn parse_uint(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let start = *cursor;
    while bytes.get(*cursor).is_some_and(u8::is_ascii_digit) {
        *cursor += 1;
    }
    std::str::from_utf8(bytes.get(start..*cursor)?)
        .ok()?
        .parse()
        .ok()
}

fn parse_line<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    let start = *cursor;
    let newline = bytes[*cursor..].iter().position(|byte| *byte == b'\n')? + *cursor;
    *cursor = newline + 1;
    Some(&bytes[start..newline])
}

fn read_until<'a>(bytes: &'a [u8], cursor: &mut usize, delimiter: u8) -> Option<&'a [u8]> {
    let start = *cursor;
    let end = bytes
        .get(*cursor..)?
        .iter()
        .position(|byte| *byte == delimiter)?
        + *cursor;
    *cursor = end + 1;
    Some(&bytes[start..end])
}

fn length_prefixed<'a>(bytes: &'a [u8], cursor: &mut usize, delimiter: u8) -> Option<&'a [u8]> {
    let length = parse_uint(bytes, cursor)? as usize;
    expect_literal(bytes, cursor, b":")?;
    let end = cursor.checked_add(length)?;
    let value = bytes.get(*cursor..end)?;
    *cursor = end;
    expect_literal(bytes, cursor, &[delimiter])?;
    Some(value)
}

fn read_sidecar_contents(root: &Path) -> Option<Vec<u8>> {
    let hash = root.file_name()?.to_string_lossy();
    let candidates = [
        root.join(".deps"),
        root.parent()?.join(format!("{hash}.deps")),
    ];
    for candidate in candidates {
        if candidate.is_file() {
            return std::fs::read(candidate).ok();
        }
    }
    None
}

fn read_deps_json_contents(root: &Path) -> Option<String> {
    let candidates = [
        root.join(".deps.json"),
        root.parent()
            .and_then(|parent| root.file_name().map(|name| (parent, name)))
            .map(|(parent, name)| parent.join(format!("{}.deps.json", name.to_string_lossy())))
            .unwrap_or_default(),
    ];
    for candidate in candidates {
        if candidate.is_file() {
            if let Ok(contents) = std::fs::read_to_string(&candidate) {
                return Some(contents);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_current_line_oriented_package_sidecar() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("packages").join("Hash123");
        std::fs::create_dir_all(&root).expect("package dir");
        let sidecar = "roc-package-deps 5\nkind package\nexpanded_bytes 10\ncontent_bytes 5\nroot_source_hash 0000000000000000000000000000000000000000000000000000000000000000\ndeps 2\ndep 1 - 2:pf 14:../pf/main.roc\ndep 0 - 4:util 40:https://example.com/u/1.0.0/hash.tar.zst\n";
        std::fs::write(temp.path().join("packages/Hash123.deps"), sidecar).expect("sidecar");
        let deps = read_deps_json(&root);
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].0, "pf");
        assert!(deps[0].2);
        assert_eq!(deps[0].1.as_str(), "../pf/main.roc");
        assert_eq!(deps[1].0, "util");
        assert_eq!(
            deps[1].1.as_str(),
            "https://example.com/u/1.0.0/hash.tar.zst"
        );
    }

    #[test]
    fn bundle_hash_strips_all_bundle_extensions() {
        let hash = "X73hGh05nNTkDHU06FHC0YfFaQB1pimX7gncRcao5mU";
        assert_eq!(
            bundle_hash(&format!("https://example.com/{hash}.tar.br")),
            Some(hash.to_string())
        );
        assert_eq!(
            bundle_hash(&format!("https://example.com/{hash}.tar.zst?download=1")),
            Some(hash.to_string())
        );
        assert_eq!(
            bundle_hash(&format!("https://example.com/{hash}")),
            Some(hash.to_string())
        );
    }
}

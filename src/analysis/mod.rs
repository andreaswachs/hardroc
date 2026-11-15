//! Semantic analysis for Roc source files and workspaces.
//!
//! The analysis layer owns three things features must not duplicate:
//!
//! * a per-file symbol table and lexical scope tree,
//! * resolution of every identifier occurrence to a definition key,
//! * module and package resolution across the workspace.
//!
//! Definition keys are stable, serializable identities used by goto,
//! references, and rename.  Local names are identified by their declaration
//! offset; top-level names by file and qualified name so an imported item still
//! resolves to the same key from another module.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use lsp_types::Url;

use crate::syntax::ast::{Definition, Module, ParsedFile};
use crate::syntax::token::{Token, TokenKind};
use crate::text::{SourceFile, TextRange};

pub mod builtins;
pub mod diag;
pub mod host;
pub mod index;
pub mod local;
pub mod packages;

pub use crate::syntax::ast::{
    Import, ImportPath, TypeDecl, TypeDeclKind, TypeExpr, TypeField, TypeKind, TypeTag,
};
pub use builtins::{BuiltinIndex, BuiltinItem, BuiltinKind, BuiltinModule};
pub use host::AnalysisHost;
pub use index::{Export, ModuleKind, ModuleSummary, WorkspaceIndex};
pub use packages::{DependencyInfo, PackageInfo, PackageKind};

/// Stable identity of a definition across the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DefinitionKey {
    /// A parameter, pattern binding, or block-local definition.
    Local { uri: Url, def_start: u32 },
    /// A top-level or associated declaration.  `container` is the enclosing
    /// nominal type when the item is a method or nested type.
    TopLevel {
        uri: Url,
        name: String,
        container: Option<String>,
    },
    /// A compiler builtin (`Str.concat`, `List`, `Ok`, ...).
    Builtin { full_name: String },
    /// A module file imported with `import`.
    Module { uri: Url },
}

impl DefinitionKey {
    pub fn is_writable_in(&self, uri: &Url) -> bool {
        match self {
            Self::Local { uri: target, .. } | Self::TopLevel { uri: target, .. } => target == uri,
            Self::Module { uri: target } => target == uri,
            Self::Builtin { .. } => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Module,
    Package,
    Type,
    Function,
    Method,
    Constant,
    Variable,
    Field,
    Tag,
    TypeParameter,
    Parameter,
    Local,
    Import,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub range: TextRange,
    pub text: String,
    pub key: Option<DefinitionKey>,
    pub kind: SymbolKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScopeInfo {
    pub id: u32,
    pub range: TextRange,
    pub parent: Option<u32>,
    pub bindings: Vec<SymbolInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: SymbolKind,
    pub name_range: TextRange,
    pub full_range: TextRange,
    pub detail: Option<String>,
    pub doc: Option<String>,
    pub container: Option<String>,
    pub key: DefinitionKey,
    pub scope: u32,
    pub children: Vec<SymbolInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamInfo {
    pub name: String,
    pub ty: Option<String>,
    pub name_range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub params: Vec<ParamInfo>,
    pub return_type: Option<String>,
    pub display: String,
    pub is_effectful: bool,
    pub type_params: Vec<String>,
    pub doc: Option<String>,
    pub key: DefinitionKey,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedImport {
    /// The effective qualifier introduced by the import (`Color`, alias, package
    /// shorthand + module, or file alias).
    pub qualifier: String,
    pub import: Import,
    pub target: Option<ModuleSummary>,
    pub file_uri: Option<Url>,
    pub exposing: Vec<String>,
    /// Type/value segments after the imported module, for nested imports such
    /// as `import json.Parser.ParseErr`.
    pub nested: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestedEdit {
    pub uri: Url,
    pub range: TextRange,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisDiagnostic {
    pub range: TextRange,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub code: String,
    pub related: Vec<(Url, TextRange, String)>,
    pub fixes: Vec<DiagnosticFix>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticFix {
    pub title: String,
    pub edits: Vec<SuggestedEdit>,
}

impl AnalysisDiagnostic {
    pub fn error(range: TextRange, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            range,
            severity: DiagnosticSeverity::Error,
            message: message.into(),
            code: code.into(),
            related: Vec::new(),
            fixes: Vec::new(),
        }
    }

    pub fn warning(range: TextRange, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            range,
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
            code: code.into(),
            related: Vec::new(),
            fixes: Vec::new(),
        }
    }
}

/// A fully analyzed file.
#[derive(Debug)]
pub struct AnalyzedFile {
    pub uri: Url,
    pub source: Arc<SourceFile>,
    pub parsed: Arc<ParsedFile>,
    pub module_name: String,
    pub module_kind: ModuleKind,
    pub symbols: Vec<SymbolInfo>,
    pub references: Vec<Reference>,
    pub scopes: Vec<ScopeInfo>,
    pub imports: Vec<ResolvedImport>,
    pub diagnostics: Vec<AnalysisDiagnostic>,
    pub signatures: HashMap<DefinitionKey, Signature>,
}

impl AnalyzedFile {
    pub fn token_at(&self, offset: u32) -> Option<&Token> {
        self.parsed
            .tokens
            .iter()
            .find(|token| token.range.contains(offset) && !token.kind.is_trivia())
            .or_else(|| {
                self.parsed
                    .tokens
                    .iter()
                    .find(|token| token.range.contains_inclusive(offset))
            })
    }

    pub fn word_at(&self, offset: u32) -> Option<(String, TextRange)> {
        let token = self.token_at(offset)?;
        if !matches!(
            token.kind,
            TokenKind::LowerIdent | TokenKind::UpperIdent | TokenKind::Keyword
        ) {
            return None;
        }
        Some((token.text(self.source.text()).to_string(), token.range))
    }

    pub fn reference_at(&self, offset: u32) -> Option<&Reference> {
        self.references
            .iter()
            .find(|reference| reference.range.contains(offset))
            .or_else(|| {
                self.references
                    .iter()
                    .find(|reference| reference.range.contains_inclusive(offset))
            })
    }

    pub fn symbol_at(&self, offset: u32) -> Option<&SymbolInfo> {
        fn visit<'a>(symbols: &'a [SymbolInfo], offset: u32, best: &mut Option<&'a SymbolInfo>) {
            for symbol in symbols {
                if symbol.name_range.contains_inclusive(offset)
                    && best.map_or(true, |current| {
                        symbol.name_range.len() < current.name_range.len()
                    })
                {
                    *best = Some(symbol);
                }
                visit(&symbol.children, offset, best);
            }
        }
        let mut best = None;
        visit(&self.symbols, offset, &mut best);
        best
    }

    pub fn definition_key_at(&self, offset: u32) -> Option<&DefinitionKey> {
        if let Some(reference) = self.reference_at(offset) {
            return reference.key.as_ref();
        }
        self.symbol_at(offset).map(|symbol| &symbol.key)
    }

    pub fn signature_for(&self, key: &DefinitionKey) -> Option<&Signature> {
        self.signatures.get(key)
    }

    pub fn enclosing_scope(&self, offset: u32) -> Option<&ScopeInfo> {
        self.scopes
            .iter()
            .filter(|scope| scope.range.contains_inclusive(offset))
            .min_by_key(|scope| scope.range.len())
    }

    /// Bindings visible at `offset`, innermost scope first.
    pub fn visible_bindings(&self, offset: u32) -> Vec<&SymbolInfo> {
        let mut scopes: Vec<&ScopeInfo> = self
            .scopes
            .iter()
            .filter(|scope| scope.range.contains_inclusive(offset))
            .collect();
        scopes.sort_by_key(|scope| scope.range.len());
        let mut output = Vec::new();
        for scope in scopes {
            for binding in &scope.bindings {
                if !output
                    .iter()
                    .any(|existing: &&SymbolInfo| existing.name == binding.name)
                {
                    output.push(binding);
                }
            }
        }
        output
    }

    pub fn module(&self) -> &Module {
        &self.parsed.module
    }

    pub fn type_decls(&self) -> Vec<&TypeDecl> {
        let mut output = Vec::new();
        fn visit<'a>(items: &'a [crate::syntax::ast::Item], output: &mut Vec<&'a TypeDecl>) {
            for item in items {
                if let crate::syntax::ast::ItemKind::TypeDecl(decl) = &item.kind {
                    output.push(decl);
                    visit(&decl.members, output);
                }
            }
        }
        visit(&self.parsed.module.items, &mut output);
        output
    }

    pub fn definitions(&self) -> Vec<&Definition> {
        let mut output = Vec::new();
        fn visit<'a>(items: &'a [crate::syntax::ast::Item], output: &mut Vec<&'a Definition>) {
            for item in items {
                match &item.kind {
                    crate::syntax::ast::ItemKind::Definition(definition) => output.push(definition),
                    crate::syntax::ast::ItemKind::TypeDecl(decl) => visit(&decl.members, output),
                    _ => {}
                }
            }
        }
        visit(&self.parsed.module.items, &mut output);
        output
    }
}

/// Configuration that affects analysis.  Runtime/server settings live in the
/// server config; this is the subset analysis needs to stay deterministic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisConfig {
    /// Maximum number of source files indexed from the workspace.
    pub max_workspace_files: usize,
    /// Whether to read package sources from the compiler cache.
    pub inspect_packages: bool,
    /// Extra directories (relative to the workspace root, or absolute) whose
    /// Roc files should be indexed in addition to the workspace tree.
    pub include_paths: Vec<String>,
    /// Path prefixes, relative to the workspace root, to leave out of the
    /// workspace index.
    pub exclude_paths: Vec<String>,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        Self {
            max_workspace_files: 20_000,
            inspect_packages: true,
            include_paths: Vec::new(),
            exclude_paths: vec![
                "target".to_string(),
                ".git".to_string(),
                "node_modules".to_string(),
            ],
        }
    }
}

/// Convenience container for the root path and root module used to resolve the
/// package graph.
#[derive(Debug, Clone)]
pub struct WorkspaceRoot {
    pub root: PathBuf,
    pub root_uri: Option<Url>,
    pub entry: Option<PathBuf>,
}

impl WorkspaceRoot {
    pub fn new(root: PathBuf) -> Self {
        let entry = find_entry(&root);
        let root_uri = Url::from_directory_path(&root).ok();
        Self {
            root,
            root_uri,
            entry,
        }
    }
}

fn find_entry(root: &std::path::Path) -> Option<PathBuf> {
    for name in ["main.roc", "app.roc", "package.roc"] {
        let candidate = root.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Shared accessors implemented by [`AnalysisHost`].
pub trait AnalysisProvider: Send + Sync {
    fn analysis(&self, uri: &Url) -> Option<Arc<AnalyzedFile>>;
    fn builtins(&self) -> &BuiltinIndex;
    fn workspace(&self) -> Arc<WorkspaceIndex>;
}

/// A helper the server and tests share: get the enclosing word/offset pair.
pub fn identifier_at(source: &SourceFile, offset: u32) -> Option<(String, TextRange)> {
    let text = source.text();
    if source.is_empty() {
        return None;
    }
    let offset = offset.min(source.len());
    let bytes = text.as_bytes();
    let is_ident = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'!';
    let mut start = offset as usize;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = offset as usize;
    while end < bytes.len() && is_ident(bytes[end]) {
        end += 1;
    }
    if start == end {
        return None;
    }
    let value = &text[start..end];
    if value == "_" {
        return None;
    }
    Some((value.to_string(), TextRange::new(start as u32, end as u32)))
}

/// A process-wide empty builtin index used only if the embedded database cannot
/// be loaded.  The real index is loaded by [`BuiltinIndex::load_embedded`].
pub fn empty_builtins() -> BuiltinIndex {
    BuiltinIndex::empty()
}

/// Re-exported for feature modules.
pub type SharedAnalysis = Arc<AnalyzedFile>;

/// Placeholder used by the server before the first workspace scan.
pub fn shared_empty_workspace(root: PathBuf) -> Arc<WorkspaceIndex> {
    Arc::new(WorkspaceIndex::empty(root))
}

/// Access to the workspace analysis used by rename/references.
pub struct WorkspaceContext {
    pub host: Arc<AnalysisHost>,
}

impl WorkspaceContext {
    pub fn new(host: Arc<AnalysisHost>) -> Self {
        Self { host }
    }

    pub fn analyses(&self) -> Vec<Arc<AnalyzedFile>> {
        self.host.all_analyses()
    }
}

/// A lock guard helper used by the host; exposed so tests can build hosts
/// without duplicating synchronization details.
pub type SharedWorkspace = Arc<RwLock<Arc<WorkspaceIndex>>>;

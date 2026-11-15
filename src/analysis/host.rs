//! Analysis host: owns documents, the workspace index, the builtin index, and
//! the analysis cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use lsp_types::Url;

use crate::syntax::parse;
use crate::text::{SourceFile, TextRange};

use super::builtins::BuiltinIndex;
use super::index::WorkspaceIndex;
use super::local;
use super::{AnalysisConfig, AnalysisProvider, AnalyzedFile, DefinitionKey, WorkspaceRoot};

#[derive(Debug)]
struct CacheEntry {
    text_hash: u64,
    generation: u64,
    analysis: Arc<AnalyzedFile>,
}

/// Shared owner for all analysis state.  Cheap to clone via `Arc<AnalysisHost>`
/// from the server.
pub struct AnalysisHost {
    root: PathBuf,
    roots: RwLock<Vec<PathBuf>>,
    config: RwLock<AnalysisConfig>,
    builtins: BuiltinIndex,
    workspace: RwLock<Arc<WorkspaceIndex>>,
    documents: RwLock<HashMap<Url, String>>,
    cache: RwLock<HashMap<Url, CacheEntry>>,
    generation: RwLock<u64>,
}

impl AnalysisHost {
    pub fn new(root: PathBuf, config: AnalysisConfig) -> Self {
        Self::new_multi(vec![root], config)
    }

    pub fn new_multi(roots: Vec<PathBuf>, config: AnalysisConfig) -> Self {
        let builtins = BuiltinIndex::load_embedded();
        let root = roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));
        let workspace = Arc::new(WorkspaceIndex::build_many(&roots, &config));
        Self {
            root,
            roots: RwLock::new(roots),
            config: RwLock::new(config),
            builtins,
            workspace: RwLock::new(workspace),
            documents: RwLock::new(HashMap::new()),
            cache: RwLock::new(HashMap::new()),
            generation: RwLock::new(1),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots
            .read()
            .map(|roots| roots.clone())
            .unwrap_or_default()
    }

    pub fn is_workspace_path(&self, path: &Path) -> bool {
        self.roots().iter().any(|root| path.starts_with(root))
    }

    pub fn config(&self) -> AnalysisConfig {
        self.config
            .read()
            .map(|config| config.clone())
            .unwrap_or_default()
    }

    pub fn update_config(&self, config: AnalysisConfig) -> Arc<WorkspaceIndex> {
        if self.config() == config {
            return self.workspace();
        }
        if let Ok(mut stored) = self.config.write() {
            *stored = config;
        }
        self.rebuild_workspace()
    }

    pub fn set_roots(&self, roots: Vec<PathBuf>) -> Arc<WorkspaceIndex> {
        if let Ok(mut stored) = self.roots.write() {
            *stored = roots;
        }
        self.rebuild_workspace()
    }

    pub fn workspace_root(&self) -> WorkspaceRoot {
        let root = self
            .roots()
            .first()
            .cloned()
            .unwrap_or_else(|| self.root.clone());
        WorkspaceRoot::new(root)
    }

    /// Pick the conventional entry module that owns `uri` in a multi-root
    /// workspace, falling back to the primary entry and then the file itself.
    pub fn entry_for_uri(&self, uri: &Url) -> Option<PathBuf> {
        let path = uri.to_file_path().ok();
        if let Some(path) = &path {
            let mut roots = self.roots();
            roots.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
            if let Some(root) = roots.iter().find(|root| path.starts_with(root)) {
                for name in ["main.roc", "app.roc", "package.roc"] {
                    let candidate = root.join(name);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
        self.workspace_root().entry.or_else(|| path.clone())
    }

    pub fn builtins(&self) -> &BuiltinIndex {
        &self.builtins
    }

    pub fn workspace(&self) -> Arc<WorkspaceIndex> {
        self.workspace
            .read()
            .map(|guard| Arc::clone(&guard))
            .unwrap_or_else(|_| Arc::new(WorkspaceIndex::empty(self.root.clone())))
    }

    pub fn replace_workspace(&self, workspace: WorkspaceIndex) {
        if let Ok(mut guard) = self.workspace.write() {
            *guard = Arc::new(workspace);
        }
        self.bump_generation();
    }

    pub fn rebuild_workspace(&self) -> Arc<WorkspaceIndex> {
        let workspace = Arc::new(WorkspaceIndex::build_many(&self.roots(), &self.config()));
        if let Ok(mut guard) = self.workspace.write() {
            *guard = Arc::clone(&workspace);
        }
        self.bump_generation();
        workspace
    }

    fn bump_generation(&self) {
        if let Ok(mut guard) = self.generation.write() {
            *guard = guard.saturating_add(1);
        }
        if let Ok(mut cache) = self.cache.write() {
            cache.clear();
        }
    }

    fn generation(&self) -> u64 {
        self.generation.read().map(|guard| *guard).unwrap_or(1)
    }

    pub fn set_document(&self, uri: Url, text: String) {
        let changed = self
            .documents
            .read()
            .ok()
            .and_then(|documents| documents.get(&uri).map(|old| old != &text))
            .unwrap_or(true);
        if let Ok(mut documents) = self.documents.write() {
            documents.insert(uri, text);
        }
        if changed {
            self.bump_document_generation();
        }
    }

    fn bump_document_generation(&self) {
        // Document edits share the generation counter with workspace rescans;
        // both invalidate cross-file analyses.
        self.bump_generation();
    }

    pub fn remove_document(&self, uri: &Url) {
        if let Ok(mut documents) = self.documents.write() {
            documents.remove(uri);
        }
        if let Ok(mut cache) = self.cache.write() {
            cache.remove(uri);
        }
        self.bump_document_generation();
    }

    pub fn document_text(&self, uri: &Url) -> Option<String> {
        self.documents
            .read()
            .ok()
            .and_then(|documents| documents.get(uri).cloned())
    }

    pub fn analyze_text(&self, uri: &Url, text: &str) -> Arc<AnalyzedFile> {
        let hash = hash_text(text);
        let generation = self.generation();
        if let Ok(cache) = self.cache.read() {
            if let Some(entry) = cache.get(uri) {
                if entry.text_hash == hash && entry.generation == generation {
                    return Arc::clone(&entry.analysis);
                }
            }
        }

        let source = Arc::new(SourceFile::new(uri.clone(), text.to_string()));
        let parsed = Arc::new(parse(text));
        let workspace = self.workspace();
        let config = self.config();
        let analysis = Arc::new(local::analyze(
            uri.clone(),
            source,
            parsed,
            &workspace,
            &self.builtins,
            &config,
        ));

        if let Ok(mut cache) = self.cache.write() {
            cache.insert(
                uri.clone(),
                CacheEntry {
                    text_hash: hash,
                    generation,
                    analysis: Arc::clone(&analysis),
                },
            );
        }
        analysis
    }

    pub fn analyze(&self, uri: &Url) -> Option<Arc<AnalyzedFile>> {
        if let Some(text) = self.document_text(uri) {
            return Some(self.analyze_text(uri, &text));
        }
        if let Ok(cache) = self.cache.read() {
            if let Some(entry) = cache.get(uri) {
                return Some(Arc::clone(&entry.analysis));
            }
        }
        let path = uri.to_file_path().ok()?;
        let text = std::fs::read_to_string(path).ok()?;
        Some(self.analyze_text(uri, &text))
    }

    pub fn all_analyses(&self) -> Vec<Arc<AnalyzedFile>> {
        let mut output = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let documents = self
            .documents
            .read()
            .map(|documents| documents.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for uri in documents {
            if let Some(analysis) = self.analyze(&uri) {
                seen.insert(uri);
                output.push(analysis);
            }
        }
        for module in self.workspace().modules.iter() {
            if seen.contains(&module.uri) {
                continue;
            }
            if let Some(analysis) = self.analyze(&module.uri) {
                output.push(analysis);
            }
        }
        output
    }

    pub fn find_definition(&self, key: &DefinitionKey) -> Option<(Url, TextRange)> {
        match key {
            DefinitionKey::Local { uri, def_start } => {
                let analysis = self.analyze(uri)?;
                let found = find_symbol(&analysis.symbols, &|symbol| {
                    symbol.name_range.start == *def_start
                })
                .map(|symbol| symbol.name_range)
                .or_else(|| {
                    analysis
                        .scopes
                        .iter()
                        .flat_map(|scope| scope.bindings.iter())
                        .find(|binding| binding.name_range.start == *def_start)
                        .map(|binding| binding.name_range)
                })
                .or_else(|| {
                    analysis
                        .references
                        .iter()
                        .find(|reference| reference.range.start == *def_start)
                        .map(|reference| reference.range)
                });
                Some((
                    uri.clone(),
                    found.unwrap_or_else(|| TextRange::empty(*def_start)),
                ))
            }
            DefinitionKey::TopLevel {
                uri,
                name,
                container,
            } => {
                let analysis = self.analyze(uri)?;
                find_symbol(&analysis.symbols, &|symbol| {
                    symbol.name == *name
                        && match container {
                            Some(container) => {
                                symbol.container.as_deref() == Some(container.as_str())
                            }
                            None => symbol.container.is_none(),
                        }
                })
                .map(|symbol| (uri.clone(), symbol.name_range))
            }
            DefinitionKey::Module { uri } => Some((uri.clone(), TextRange::empty(0))),
            DefinitionKey::Builtin { .. } => None,
        }
    }

    /// Every occurrence of `key` across open documents and indexed modules.
    pub fn references_to_key(&self, key: &DefinitionKey) -> Vec<(Url, TextRange)> {
        let mut output = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for analysis in self.all_analyses() {
            for reference in &analysis.references {
                if reference.key.as_ref() != Some(key) {
                    continue;
                }
                if seen.insert((analysis.uri.clone(), reference.range)) {
                    output.push((analysis.uri.clone(), reference.range));
                }
            }
        }
        if let Some((uri, range)) = self.find_definition(key) {
            if seen.insert((uri.clone(), range)) {
                output.push((uri, range));
            }
        }
        output.sort_by(|left, right| {
            left.0
                .as_str()
                .cmp(right.0.as_str())
                .then(left.1.start.cmp(&right.1.start))
        });
        output
    }

    /// Symbols from every analyzed file, grouped by document.  Used by
    /// workspace-symbol search; callers may narrow the result by name.
    pub fn workspace_symbols(&self) -> Vec<(Url, super::SymbolInfo)> {
        let mut output = Vec::new();
        for analysis in self.all_analyses() {
            for symbol in &analysis.symbols {
                let mut stack = vec![symbol];
                while let Some(symbol) = stack.pop() {
                    output.push((analysis.uri.clone(), symbol.clone()));
                    for child in &symbol.children {
                        stack.push(child);
                    }
                }
            }
        }
        output
    }

    /// Resolve a module import string from `from` without a full analysis.
    pub fn resolve_module(&self, from: &Url, path: &str) -> Option<super::ModuleSummary> {
        let workspace = self.workspace();
        if let Ok(from_path) = from.to_file_path() {
            if let Some(directory) = from_path.parent() {
                for candidate in [path.to_string(), path.replace('.', "/")] {
                    let file = if candidate.ends_with(".roc") {
                        directory.join(candidate)
                    } else {
                        directory.join(format!("{candidate}.roc"))
                    };
                    let candidate = crate::analysis::packages::normalize_path(&file);
                    if let Some(module) = workspace.module_for_path(&candidate) {
                        return Some(module.clone());
                    }
                }
            }
        }
        let raw = path.trim_end_matches(".roc");
        let mut parts = raw.split(['.', '/']);
        let first = parts.next().unwrap_or(raw);
        if workspace.package(first).is_some() {
            let module = raw
                .split_once(['.', '/'])
                .map(|(_, rest)| rest)
                .unwrap_or(raw);
            if let Some(module) = workspace.find_public_module(first, module) {
                return Some(module.clone());
            }
        }
        workspace.find_module(None, raw).cloned()
    }
}

fn find_symbol<'a>(
    symbols: &'a [super::SymbolInfo],
    predicate: &dyn Fn(&super::SymbolInfo) -> bool,
) -> Option<&'a super::SymbolInfo> {
    for symbol in symbols {
        if predicate(symbol) {
            return Some(symbol);
        }
        if let Some(found) = find_symbol(&symbol.children, predicate) {
            return Some(found);
        }
    }
    None
}

impl AnalysisProvider for AnalysisHost {
    fn analysis(&self, uri: &Url) -> Option<Arc<AnalyzedFile>> {
        self.analyze(uri)
    }

    fn builtins(&self) -> &BuiltinIndex {
        &self.builtins
    }

    fn workspace(&self) -> Arc<WorkspaceIndex> {
        AnalysisHost::workspace(self)
    }
}

fn hash_text(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

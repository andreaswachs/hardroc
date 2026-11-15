//! Workspace index: modules, exports, and package dependency metadata.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lsp_types::Url;
use walkdir::WalkDir;

use crate::analysis::packages::{
    self, normalize_path, package_root_file, read_deps_json, resolve_dependency_file,
    resolve_dependency_root,
};
use crate::analysis::{
    AnalysisConfig, DefinitionKey, DependencyInfo, PackageInfo, PackageKind, SymbolKind,
};
use crate::syntax::ast::{
    self, Dependency, DependencySource, ExprKind, HeaderKind, Item, ItemKind, ModuleHeader,
    TypeDecl,
};
use crate::syntax::parse;
use crate::text::TextRange;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    App,
    Package,
    Platform,
    TypeModule,
    Module,
}

impl ModuleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Package => "package",
            Self::Platform => "platform",
            Self::TypeModule => "type module",
            Self::Module => "module",
        }
    }
}

/// An item visible from a module.  Nested items (methods of a type) are
/// children, mirroring Roc's associated-item model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    pub kind: SymbolKind,
    pub detail: Option<String>,
    pub doc: Option<String>,
    pub selection_range: TextRange,
    pub full_range: TextRange,
    pub container: Option<String>,
    pub key: Option<DefinitionKey>,
    pub children: Vec<Export>,
}

impl Export {
    pub fn walk<'a>(&'a self, output: &mut Vec<&'a Export>) {
        output.push(self);
        for child in &self.children {
            child.walk(output);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSummary {
    pub uri: Url,
    pub path: PathBuf,
    /// Importable module name (file stem, or package-relative path).
    pub name: String,
    pub kind: ModuleKind,
    pub exports: Vec<Export>,
    pub dependencies: Vec<DependencyInfo>,
    pub header_kind: Option<HeaderKind>,
    pub header_range: Option<TextRange>,
    pub doc: Option<String>,
    /// Package shorthand this module belongs to, if any.
    pub package: Option<String>,
    /// Package-root path (main.roc / app.roc / platform main.roc).
    pub package_root: Option<PathBuf>,
}

impl ModuleSummary {
    pub fn all_exports(&self) -> Vec<&Export> {
        let mut output = Vec::new();
        for export in &self.exports {
            export.walk(&mut output);
        }
        output
    }

    pub fn export(&self, name: &str) -> Option<&Export> {
        self.all_exports()
            .into_iter()
            .find(|export| export.name == name)
    }

    pub fn export_for_key(&self, key: &DefinitionKey) -> Option<&Export> {
        self.all_exports()
            .into_iter()
            .find(|export| export.key.as_ref() == Some(key))
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceIndex {
    pub root: PathBuf,
    pub root_uri: Option<Url>,
    pub entry: Option<PathBuf>,
    pub modules: Vec<ModuleSummary>,
    pub packages: Vec<PackageInfo>,
    /// Public import name for package modules. Private modules are absent.
    pub public_names: HashMap<PathBuf, String>,
    by_uri: HashMap<Url, usize>,
    by_path: HashMap<PathBuf, usize>,
}

impl WorkspaceIndex {
    /// Scan a workspace for `.roc` files, module headers, and packages.
    pub fn build(root: &Path, config: &AnalysisConfig) -> Self {
        let mut root = normalize_path(root);
        if root.as_os_str().is_empty() {
            root = PathBuf::from(".");
        }
        let entry = ["main.roc", "app.roc", "package.roc"]
            .iter()
            .map(|name| root.join(name))
            .find(|path| path.is_file());
        let mut builder = Builder::new(root.clone(), config);
        builder.scan_workspace();
        builder.discover_packages();
        WorkspaceIndex::new_with_public(
            root,
            entry,
            builder.modules,
            builder.packages,
            builder.public_names,
        )
    }

    /// Build a merged index for a multi-root workspace.
    pub fn build_many(roots: &[PathBuf], config: &AnalysisConfig) -> Self {
        let mut normalized = Vec::new();
        for root in roots {
            let root = normalize_path(root);
            if !root.as_os_str().is_empty() && !normalized.contains(&root) {
                normalized.push(root);
            }
        }
        let Some(primary) = normalized.first().cloned() else {
            return WorkspaceIndex::empty(PathBuf::from("."));
        };

        let mut modules = Vec::new();
        let mut packages = Vec::new();
        let mut public_names = HashMap::new();
        let mut entry = None;
        for root in &normalized {
            let index = WorkspaceIndex::build(root, config);
            if entry.is_none() {
                entry = index.entry.clone();
            }
            for module in index.modules {
                if !modules
                    .iter()
                    .any(|existing: &ModuleSummary| existing.path == module.path)
                {
                    modules.push(module);
                }
            }
            public_names.extend(index.public_names);
            for package in index.packages {
                if let Some(existing) = packages
                    .iter_mut()
                    .find(|existing: &&mut PackageInfo| existing.root == package.root)
                {
                    for alias in std::iter::once(package.shorthand.clone())
                        .chain(package.aliases.iter().cloned())
                    {
                        if !existing.has_alias(&alias) {
                            existing.aliases.push(alias);
                        }
                    }
                    if existing.dependencies.is_empty() {
                        existing.dependencies = package.dependencies;
                    }
                } else {
                    packages.push(package);
                }
            }
        }
        WorkspaceIndex::new_with_public(primary, entry, modules, packages, public_names)
    }

    #[allow(dead_code)]
    fn empty_with_entry(root: PathBuf, entry: Option<PathBuf>) -> Self {
        let mut index = Self::empty(root);
        index.entry = entry;
        index
    }

    pub fn empty(root: PathBuf) -> Self {
        let root_uri = Url::from_directory_path(&root).ok();
        Self {
            root,
            root_uri,
            entry: None,
            modules: Vec::new(),
            packages: Vec::new(),
            public_names: HashMap::new(),
            by_uri: HashMap::new(),
            by_path: HashMap::new(),
        }
    }

    pub fn new(
        root: PathBuf,
        entry: Option<PathBuf>,
        modules: Vec<ModuleSummary>,
        packages: Vec<PackageInfo>,
    ) -> Self {
        Self::new_with_public(root, entry, modules, packages, HashMap::new())
    }

    pub fn new_with_public(
        root: PathBuf,
        entry: Option<PathBuf>,
        modules: Vec<ModuleSummary>,
        packages: Vec<PackageInfo>,
        public_names: HashMap<PathBuf, String>,
    ) -> Self {
        let mut by_uri = HashMap::new();
        let mut by_path = HashMap::new();
        for (index, module) in modules.iter().enumerate() {
            by_uri.insert(module.uri.clone(), index);
            by_path.insert(module.path.clone(), index);
        }
        let root_uri = Url::from_directory_path(&root).ok();
        Self {
            root,
            root_uri,
            entry,
            modules,
            packages,
            public_names,
            by_uri,
            by_path,
        }
    }

    pub fn public_name_for_path(&self, path: &Path) -> Option<&str> {
        self.public_names.get(path).map(String::as_str)
    }

    pub fn is_package_root(&self, path: &Path) -> bool {
        self.packages
            .iter()
            .any(|package| package.root_file.as_deref() == Some(path))
    }

    /// Find a package module by its public import name. Private module paths
    /// are intentionally not considered.
    pub fn find_public_module(&self, package: &str, module: &str) -> Option<&ModuleSummary> {
        let wanted = canonical_module_name(module);
        let roots: Vec<&Path> = self
            .packages
            .iter()
            .filter(|info| info.has_alias(package))
            .filter_map(|info| info.root.as_deref())
            .collect();
        self.modules.iter().find(|candidate| {
            let in_package = candidate.package.as_deref() == Some(package)
                || candidate
                    .package_root
                    .as_deref()
                    .is_some_and(|root| roots.contains(&root));
            in_package
                && self
                    .public_names
                    .get(&candidate.path)
                    .is_some_and(|name| canonical_module_name(name) == wanted || name == module)
        })
    }

    pub fn module_for_uri(&self, uri: &Url) -> Option<&ModuleSummary> {
        self.by_uri
            .get(uri)
            .and_then(|index| self.modules.get(*index))
    }

    pub fn module_for_path(&self, path: &Path) -> Option<&ModuleSummary> {
        self.by_path
            .get(path)
            .and_then(|index| self.modules.get(*index))
    }

    pub fn module_names(&self) -> impl Iterator<Item = &str> {
        self.modules.iter().map(|module| module.name.as_str())
    }

    pub fn package(&self, shorthand: &str) -> Option<&PackageInfo> {
        self.packages
            .iter()
            .find(|package| package.has_alias(shorthand))
    }

    pub fn package_for_path(&self, path: &Path) -> Option<&PackageInfo> {
        self.packages.iter().find(|package| {
            package
                .root
                .as_deref()
                .is_some_and(|root| path.starts_with(root))
        })
    }

    /// Find a module by its importable name, optionally scoped to a package.
    pub fn find_module(&self, package: Option<&str>, module: &str) -> Option<&ModuleSummary> {
        let wanted = canonical_module_name(module);
        let matches = |candidate: &ModuleSummary| {
            canonical_module_name(&candidate.name) == wanted
                || candidate.name.as_str() == module
                || self
                    .public_names
                    .get(&candidate.path)
                    .is_some_and(|name| canonical_module_name(name) == wanted || name == module)
        };
        let package_roots: Vec<&Path> = package
            .map(|package| {
                self.packages
                    .iter()
                    .filter(|info| info.has_alias(package))
                    .filter_map(|info| info.root.as_deref())
                    .collect()
            })
            .unwrap_or_default();
        self.modules
            .iter()
            .find(|candidate| {
                matches(candidate)
                    && match package {
                        Some(package) => {
                            candidate.package.as_deref() == Some(package)
                                || candidate
                                    .package_root
                                    .as_deref()
                                    .is_some_and(|root| package_roots.contains(&root))
                        }
                        None => true,
                    }
            })
            .or_else(|| {
                // Sub-directory imports spell directories with `/` while the
                // index stores a canonical path.  Try the last segment as a
                // final fallback, but never cross package boundaries.
                if package.is_none() {
                    self.modules
                        .iter()
                        .find(|candidate| candidate.name == module)
                } else {
                    None
                }
            })
    }
}

fn canonical_module_name(name: &str) -> String {
    name.trim_end_matches(".roc")
        .replace(['.', '/'], "/")
        .trim_matches('/')
        .to_string()
}

struct Builder {
    root: PathBuf,
    max_files: usize,
    inspect_packages: bool,
    modules: Vec<ModuleSummary>,
    packages: Vec<PackageInfo>,
    public_names: HashMap<PathBuf, String>,
    by_path: HashMap<PathBuf, usize>,
    by_uri: HashMap<Url, usize>,
    header_files: Vec<PathBuf>,
    visited_packages: HashSet<PathBuf>,
    excluded: Vec<String>,
    include_roots: Vec<PathBuf>,
}

impl Builder {
    fn new(root: PathBuf, config: &AnalysisConfig) -> Self {
        let include_roots = config
            .include_paths
            .iter()
            .map(|path| {
                let path = Path::new(path);
                let joined = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    root.join(path)
                };
                normalize_path(&joined)
            })
            .filter(|path| path.is_dir() && !self_root(&root, path))
            .collect();
        Self {
            root,
            max_files: config.max_workspace_files.max(1),
            inspect_packages: config.inspect_packages,
            modules: Vec::new(),
            packages: Vec::new(),
            public_names: HashMap::new(),
            by_path: HashMap::new(),
            by_uri: HashMap::new(),
            header_files: Vec::new(),
            visited_packages: HashSet::new(),
            excluded: {
                let mut excluded = read_exclude_paths();
                excluded.extend(config.exclude_paths.iter().cloned());
                excluded.sort();
                excluded.dedup();
                excluded
            },
            include_roots,
        }
    }

    fn scan_workspace(&mut self) {
        let mut scopes = vec![self.root.clone()];
        for include in &self.include_roots {
            if !scopes.contains(include) {
                scopes.push(include.clone());
            }
        }
        for scope in scopes {
            let mut files = Vec::new();
            let walker = WalkDir::new(&scope)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| {
                    self.should_descend(entry.path(), entry.file_type().is_dir())
                });
            for entry in walker.flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                if entry.path().extension().and_then(|ext| ext.to_str()) != Some("roc") {
                    continue;
                }
                files.push(normalize_path(entry.path()));
                if self.modules.len() + files.len() >= self.max_files {
                    break;
                }
            }
            files.sort();
            for path in files {
                let name = relative_module_name(&scope, &path);
                self.add_file(path, name, None, None);
            }
        }
    }

    fn should_descend(&self, path: &Path, is_dir: bool) -> bool {
        if !is_dir || path.as_os_str() == self.root.as_os_str() {
            return true;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name.starts_with('.') {
            return false;
        }
        let relative = path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        !self
            .excluded
            .iter()
            .any(|prefix| relative.starts_with(prefix.trim_end_matches('/')))
    }

    fn add_file(
        &mut self,
        path: PathBuf,
        name: String,
        package: Option<String>,
        package_root: Option<PathBuf>,
    ) {
        let path = normalize_path(&path);
        if let Some(&index) = self.by_path.get(&path) {
            if let Some(module) = self.modules.get_mut(index) {
                if let Some(package) = package {
                    module.package = Some(package);
                    module.name = name;
                }
                if module.package_root.is_none() {
                    module.package_root = package_root;
                }
            }
            return;
        }
        if self.modules.len() >= self.max_files {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        if text.len() > 8 * 1024 * 1024 {
            return;
        }
        let Some(uri) = packages::uri_for_path(&path) else {
            return;
        };
        let parsed = parse(&text);
        let summary = summarize(
            uri.clone(),
            path.clone(),
            name,
            &text,
            &parsed.module,
            package,
            package_root,
        );
        if summary.header_kind.is_some() {
            self.header_files.push(path.clone());
        }
        let index = self.modules.len();
        self.by_path.insert(path, index);
        self.by_uri.insert(uri, index);
        self.modules.push(summary);
    }

    fn discover_packages(&mut self) {
        let headers: Vec<PathBuf> = self.header_files.clone();
        let mut apps = Vec::new();
        let mut platforms = Vec::new();
        let mut packages = Vec::new();
        for path in headers {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let parsed = parse(&text);
            let Some(header) = parsed.module.header.as_ref() else {
                continue;
            };
            match header.kind {
                HeaderKind::App => apps.push((path, header.clone())),
                HeaderKind::Platform => platforms.push((path, header.clone())),
                HeaderKind::Package => packages.push((path, header.clone())),
                HeaderKind::Module => {}
            }
        }

        // Dependencies declared by applications are authoritative: their alias
        // becomes the package's primary shorthand before any workspace
        // package/platform header invents a file-stem shorthand.
        for (path, header) in &apps {
            for dependency in &header.dependencies {
                self.add_dependency(path, dependency);
            }
        }
        for (path, header) in &platforms {
            let root = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            if !self.has_package_root(&root) {
                let shorthand = path
                    .parent()
                    .and_then(|parent| parent.file_name())
                    .and_then(|name| name.to_str())
                    .unwrap_or("platform")
                    .to_string();
                self.index_package(
                    root.clone(),
                    &shorthand,
                    DependencySource::Path(".".into()),
                    true,
                    Some(path.clone()),
                    0,
                );
            }
            for dependency in &header.dependencies {
                self.add_dependency(path, dependency);
            }
        }
        for (path, header) in &packages {
            let root = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            if !self.has_package_root(&root) {
                self.index_workspace_package(path, header);
            }
            for dependency in &header.dependencies {
                self.add_dependency(path, dependency);
            }
        }
    }

    fn has_package_root(&self, root: &Path) -> bool {
        let root = normalize_path(root);
        self.packages
            .iter()
            .any(|package| package.root.as_deref() == Some(root.as_path()))
    }

    fn index_workspace_package(&mut self, header_path: &Path, _header: &ModuleHeader) {
        let root = header_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let shorthand = header_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| *stem != "main" && *stem != "package")
            .map(str::to_string)
            .or_else(|| {
                header_path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "package".to_string());
        self.index_package(
            root,
            &shorthand,
            DependencySource::Path(
                header_path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("package")
                    .to_string(),
            ),
            false,
            Some(header_path.to_path_buf()),
            0,
        );
    }

    fn add_dependency(&mut self, declaring_file: &Path, dependency: &Dependency) {
        match &dependency.source {
            DependencySource::RocVersion(_) => return,
            DependencySource::CompilerOwned(_) => {
                if self
                    .packages
                    .iter()
                    .all(|package| !package.has_alias(&dependency.shorthand.text))
                {
                    self.packages.push(PackageInfo {
                        shorthand: dependency.shorthand.text.clone(),
                        aliases: vec![dependency.shorthand.text.clone()],
                        source: dependency.source.clone(),
                        root: None,
                        root_file: None,
                        kind: PackageKind::Platform,
                        version: None,
                        exposes: Vec::new(),
                        modules: Vec::new(),
                        dependencies: Vec::new(),
                        header_range: None,
                        error: Some(
                            "compiler-owned platform: sources are not available on disk"
                                .to_string(),
                        ),
                    });
                }
                return;
            }
            DependencySource::Url(_) if !self.inspect_packages => return,
            _ => {}
        }
        let (root, error) = resolve_dependency_root(declaring_file, &dependency.source);
        let Some(root) = root else {
            return;
        };
        if !root.is_dir() {
            if self
                .packages
                .iter()
                .all(|package| package.root.as_deref() != Some(root.as_path()))
            {
                self.packages.push(PackageInfo {
                    shorthand: dependency.shorthand.text.clone(),
                    aliases: vec![dependency.shorthand.text.clone()],
                    source: dependency.source.clone(),
                    root: Some(root),
                    root_file: resolve_dependency_file(declaring_file, &dependency.source),
                    kind: if dependency.is_platform {
                        PackageKind::Platform
                    } else {
                        PackageKind::Package
                    },
                    version: None,
                    exposes: Vec::new(),
                    modules: Vec::new(),
                    dependencies: Vec::new(),
                    header_range: None,
                    error: error
                        .or_else(|| Some("package source is not available locally".to_string())),
                });
            }
            return;
        }
        let root_file = resolve_dependency_file(declaring_file, &dependency.source);
        self.index_package(
            root,
            &dependency.shorthand.text,
            dependency.source.clone(),
            dependency.is_platform,
            root_file,
            0,
        );
    }

    fn index_package(
        &mut self,
        root: PathBuf,
        shorthand: &str,
        source: DependencySource,
        is_platform: bool,
        declared_root_file: Option<PathBuf>,
        depth: usize,
    ) -> PackageInfo {
        let root = normalize_path(&root);
        if let Some(index) = self
            .packages
            .iter()
            .position(|package| package.root.as_deref() == Some(root.as_path()))
        {
            if !self.packages[index].has_alias(shorthand) {
                self.packages[index].aliases.push(shorthand.to_string());
            }
            if self.packages[index].root_file.is_none() {
                self.packages[index].root_file = declared_root_file.clone();
            }
            for module in &mut self.modules {
                if module.package_root.as_deref() == Some(root.as_path())
                    && module.package.is_none()
                {
                    module.package = Some(self.packages[index].shorthand.clone());
                }
            }
            return self.packages[index].clone();
        }
        if depth > 8 || !root.is_dir() {
            let package = PackageInfo {
                shorthand: shorthand.to_string(),
                aliases: vec![shorthand.to_string()],
                source,
                root: Some(root),
                root_file: declared_root_file.clone(),
                kind: if is_platform {
                    PackageKind::Platform
                } else {
                    PackageKind::Package
                },
                version: None,
                exposes: Vec::new(),
                modules: Vec::new(),
                dependencies: Vec::new(),
                header_range: None,
                error: Some("package source is not available locally".to_string()),
            };
            self.packages.push(package.clone());
            return package;
        }

        let root_file = declared_root_file.or_else(|| package_root_file(&root));
        let parsed_header = root_file.as_ref().and_then(|path| {
            std::fs::read_to_string(path)
                .ok()
                .map(|text| parse(&text).module.header)
        });
        let Some(header) = parsed_header.flatten() else {
            let package = PackageInfo {
                shorthand: shorthand.to_string(),
                aliases: vec![shorthand.to_string()],
                source,
                root: Some(root),
                root_file: root_file.clone(),
                kind: if is_platform {
                    PackageKind::Platform
                } else {
                    PackageKind::Package
                },
                version: None,
                exposes: Vec::new(),
                modules: Vec::new(),
                dependencies: Vec::new(),
                header_range: None,
                error: Some("package root has no `main.roc` header".to_string()),
            };
            self.packages.push(package.clone());
            return package;
        };

        let mut package = packages::package_from_header(
            &header,
            root_file.as_deref().unwrap_or(Path::new("main.roc")),
            source,
            shorthand,
        );
        package.root = Some(root.clone());
        package.root_file = root_file.clone();
        package.error = None;
        // Fill the provided-module list from both header shapes.
        for name in &header.exposes {
            if !package.modules.contains(&name.text) {
                package.modules.push(name.text.clone());
            }
        }
        package.kind = PackageKind::from_header(header.kind);
        self.packages.push(package.clone());

        // Index every module file shipped with the package.
        let mut files = Vec::new();
        for entry in WalkDir::new(&root)
            .max_depth(6)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("roc") {
                continue;
            }
            let name = entry.file_name().to_str().unwrap_or("");
            if name.starts_with('.') {
                continue;
            }
            files.push(normalize_path(entry.path()));
        }
        files.sort();
        let public_names = package_public_names(&header, &package);
        let import_names = root_import_names(&root, root_file.as_deref(), &public_names);
        for path in &files {
            if root_file.as_deref() == Some(path.as_path()) {
                continue;
            }
            let name = relative_module_name(&root, path);
            let stem = name.rsplit('/').next().unwrap_or(&name);
            let public = import_names.get(path).cloned().or_else(|| {
                if public_names.contains(stem) {
                    Some(stem.to_string())
                } else if public_names.contains(&name) {
                    Some(name.clone())
                } else {
                    None
                }
            });
            self.add_file(
                path.clone(),
                name,
                Some(shorthand.to_string()),
                Some(root.clone()),
            );
            if let Some(public) = public {
                self.public_names.insert(path.clone(), public);
            }
        }

        // Dependencies declared in the header plus compiler bundle metadata.
        let mut dependencies: Vec<DependencyInfo> = Vec::new();
        let mut seen_aliases: HashSet<String> = HashSet::new();
        for dependency in &header.dependencies {
            if matches!(
                dependency.source,
                DependencySource::RocVersion(_) | DependencySource::CompilerOwned(_)
            ) {
                continue;
            }
            if !seen_aliases.insert(dependency.shorthand.text.clone()) {
                continue;
            }
            let info = self.make_dependency(
                &root,
                &dependency.source,
                &dependency.shorthand.text,
                dependency.is_platform,
                dependency.source_range,
                depth,
            );
            dependencies.push(info);
        }
        for (alias, dep_source, is_platform) in read_deps_json(&root) {
            if !seen_aliases.insert(alias.clone()) {
                continue;
            }
            let info = self.make_dependency(
                &root,
                &dep_source,
                &alias,
                is_platform,
                TextRange::default(),
                depth,
            );
            dependencies.push(info);
        }
        package.dependencies = dependencies.clone();
        if let Some(existing) = self
            .packages
            .iter_mut()
            .find(|existing| existing.root.as_deref() == Some(root.as_path()))
        {
            existing.dependencies = dependencies;
        }
        package
    }

    fn make_dependency(
        &mut self,
        declaring_root: &Path,
        source: &DependencySource,
        shorthand: &str,
        is_platform: bool,
        source_range: TextRange,
        depth: usize,
    ) -> DependencyInfo {
        if matches!(source, DependencySource::Url(_)) && !self.inspect_packages {
            return DependencyInfo {
                shorthand: shorthand.to_string(),
                is_platform,
                source: source.clone(),
                source_range,
                root: None,
                package: None,
                error: Some("package inspection is disabled".to_string()),
            };
        }
        let declaring = declaring_root.join("main.roc");
        let (root, error) = resolve_dependency_root(&declaring, source);
        let mut info = DependencyInfo {
            shorthand: shorthand.to_string(),
            is_platform,
            source: source.clone(),
            source_range,
            root: root.clone(),
            package: None,
            error,
        };
        if let Some(root) = root {
            if root.is_dir() && !self.visited_packages.contains(&root) {
                self.visited_packages.insert(root.clone());
                if self
                    .packages
                    .iter()
                    .all(|package| package.root.as_deref() != Some(root.as_path()))
                {
                    let root_file = resolve_dependency_file(&declaring, source);
                    let package = self.index_package(
                        root,
                        shorthand,
                        source.clone(),
                        is_platform,
                        root_file,
                        depth + 1,
                    );
                    info.package = Some(Box::new(package));
                }
            } else if let Some(package) = self
                .packages
                .iter()
                .find(|package| package.root.as_deref() == Some(root.as_path()))
            {
                info.package = Some(Box::new(package.clone()));
            }
        }
        info
    }
}

fn read_exclude_paths() -> Vec<String> {
    let mut output = vec![
        "target".to_string(),
        ".git".to_string(),
        "node_modules".to_string(),
    ];
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/exclude_paths");
    if let Ok(contents) = std::fs::read_to_string(manifest) {
        for line in contents.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                output.push(line.to_string());
            }
        }
    }
    output
}

fn self_root(root: &Path, path: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn package_public_names(header: &ModuleHeader, package: &PackageInfo) -> HashSet<String> {
    let mut names: HashSet<String> = package.exposes.iter().cloned().collect();
    names.extend(package.modules.iter().cloned());
    for name in header.exposes.iter().chain(header.provides.iter()) {
        names.insert(name.text.clone());
    }
    names
}

fn root_import_names(
    root: &Path,
    root_file: Option<&Path>,
    public_names: &HashSet<String>,
) -> HashMap<PathBuf, String> {
    let Some(root_file) = root_file else {
        return HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(root_file) else {
        return HashMap::new();
    };
    let parsed = parse(&text);
    let mut output = HashMap::new();
    for import in &parsed.module.imports {
        let ast::ImportPath::Roc { raw, .. } = &import.path else {
            continue;
        };
        let raw = raw.trim_end_matches(".roc");
        let mut candidates = vec![normalize_path(&root.join(format!("{raw}.roc")))];
        candidates.push(normalize_path(&root.join(raw).join("main.roc")));
        let Some(path) = candidates.into_iter().find(|path| path.is_file()) else {
            continue;
        };
        let fallback = raw.rsplit('/').next().unwrap_or(raw);
        let public = import
            .alias
            .as_ref()
            .map(|name| name.text.as_str())
            .unwrap_or(fallback);
        if public_names.contains(public) {
            output.insert(path, public.to_string());
        }
    }
    output
}

fn relative_module_name(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let without_extension = relative.with_extension("");
    without_extension.to_string_lossy().replace('\\', "/")
}

fn summarize(
    uri: Url,
    path: PathBuf,
    name: String,
    source: &str,
    module: &ast::Module,
    package: Option<String>,
    package_root: Option<PathBuf>,
) -> ModuleSummary {
    let header_kind = module.header.as_ref().map(|header| header.kind);
    let kind = match header_kind {
        Some(HeaderKind::App) => ModuleKind::App,
        Some(HeaderKind::Module) => ModuleKind::Module,
        Some(HeaderKind::Package) => ModuleKind::Package,
        Some(HeaderKind::Platform) => ModuleKind::Platform,
        None => {
            if type_module_decl(module, &name).is_some() {
                ModuleKind::TypeModule
            } else {
                ModuleKind::Module
            }
        }
    };
    let mut exports = match kind {
        ModuleKind::TypeModule => type_module_decl(module, &name)
            .map(|decl| vec![type_export(decl, &uri, None, source)])
            .unwrap_or_else(|| item_exports(&module.items, &uri, None, source)),
        _ => item_exports(&module.items, &uri, None, source),
    };
    if header_kind == Some(HeaderKind::Module) {
        if let Some(header) = &module.header {
            exports.retain(|export| header.provides.iter().any(|name| name.text == export.name));
        }
    }
    ModuleSummary {
        uri,
        name,
        kind,
        exports,
        dependencies: module
            .header
            .as_ref()
            .map(|header| dependency_infos(header, &path))
            .unwrap_or_default(),
        header_kind,
        header_range: module.header.as_ref().map(|header| header.range),
        doc: module_doc(source),
        package,
        package_root,
        path,
    }
}

fn module_doc(source: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in source.lines() {
        let text = line.trim();
        if let Some(doc) = text
            .strip_prefix("##")
            .filter(|rest| !rest.starts_with('#'))
        {
            lines.push(doc.trim().to_string());
        } else if text.is_empty() {
            if !lines.is_empty() {
                break;
            }
        } else {
            break;
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn type_module_decl<'a>(module: &'a ast::Module, name: &str) -> Option<&'a TypeDecl> {
    let stem = name.rsplit('/').next().unwrap_or(name);
    module.items.iter().find_map(|item| match &item.kind {
        ItemKind::TypeDecl(decl) if decl.name.text.as_str() == stem => Some(decl),
        _ => None,
    })
}

fn dependency_infos(header: &ModuleHeader, declaring_file: &Path) -> Vec<DependencyInfo> {
    header
        .dependencies
        .iter()
        .map(|dependency| {
            let (root, error) = match &dependency.source {
                DependencySource::RocVersion(_) | DependencySource::CompilerOwned(_) => {
                    (None, None)
                }
                source => resolve_dependency_root(declaring_file, source),
            };
            DependencyInfo {
                shorthand: dependency.shorthand.text.clone(),
                is_platform: dependency.is_platform,
                source: dependency.source.clone(),
                source_range: dependency.source_range,
                root,
                package: None,
                error,
            }
        })
        .collect()
}

fn item_exports(items: &[Item], uri: &Url, container: Option<&str>, source: &str) -> Vec<Export> {
    let mut output = Vec::new();
    for item in items {
        match &item.kind {
            ItemKind::Definition(definition) => {
                definition_exports(definition, uri, container, source, &mut output)
            }
            ItemKind::Annotation(annotation) => output.push(Export {
                name: annotation.name.text.clone(),
                kind: if is_function_type(&annotation.ty) {
                    SymbolKind::Function
                } else {
                    SymbolKind::Constant
                },
                detail: Some(slice(source, annotation.ty.range)),
                doc: annotation
                    .doc
                    .as_ref()
                    .map(|doc| doc.text.trim().to_string()),
                selection_range: annotation.name.range,
                full_range: annotation.range,
                container: container.map(str::to_string),
                key: Some(DefinitionKey::TopLevel {
                    uri: uri.clone(),
                    name: annotation.name.text.clone(),
                    container: container.map(str::to_string),
                }),
                children: Vec::new(),
            }),
            ItemKind::TypeDecl(decl) => output.push(type_export(decl, uri, container, source)),
            ItemKind::WhereAlias(alias) => output.push(Export {
                name: alias.name.text.clone(),
                kind: SymbolKind::Type,
                detail: Some(format!("where {} requirement(s)", alias.requirements.len())),
                doc: alias.doc.as_ref().map(|doc| doc.text.trim().to_string()),
                selection_range: alias.name.range,
                full_range: alias.range,
                container: container.map(str::to_string),
                key: Some(DefinitionKey::TopLevel {
                    uri: uri.clone(),
                    name: alias.name.text.clone(),
                    container: container.map(str::to_string),
                }),
                children: Vec::new(),
            }),
            _ => {}
        }
    }
    dedupe_exports(output)
}

fn dedupe_exports(exports: Vec<Export>) -> Vec<Export> {
    let mut output: Vec<Export> = Vec::new();
    for export in exports {
        if let Some(existing) = output
            .iter_mut()
            .find(|entry| entry.name == export.name && entry.container == export.container)
        {
            if export.selection_range.start >= existing.selection_range.start {
                let doc = export.doc.clone().or_else(|| existing.doc.clone());
                let detail = export.detail.clone().or_else(|| existing.detail.clone());
                *existing = export;
                existing.doc = doc;
                existing.detail = detail;
            }
        } else {
            output.push(export);
        }
    }
    output
}

fn definition_exports(
    definition: &ast::Definition,
    uri: &Url,
    container: Option<&str>,
    source: &str,
    output: &mut Vec<Export>,
) {
    let kind = definition_kind(definition, container.is_some());
    let detail = definition
        .annotation
        .as_ref()
        .map(|annotation| slice(source, annotation.range));
    let doc = definition
        .doc
        .as_ref()
        .map(|doc| doc.text.trim().to_string());
    if let Some(name) = definition.simple_name() {
        output.push(Export {
            name: name.text.clone(),
            kind,
            detail,
            doc,
            selection_range: name.range,
            full_range: definition.range,
            container: container.map(str::to_string),
            key: Some(DefinitionKey::TopLevel {
                uri: uri.clone(),
                name: name.text.clone(),
                container: container.map(str::to_string),
            }),
            children: Vec::new(),
        });
        return;
    }
    let mut bindings = Vec::new();
    definition.pattern.bindings(&mut bindings);
    for (name, range, _is_var) in bindings {
        output.push(Export {
            name: name.clone(),
            kind: if container.is_some() {
                SymbolKind::Method
            } else {
                SymbolKind::Constant
            },
            detail: detail.clone(),
            doc: doc.clone(),
            selection_range: range,
            full_range: definition.range,
            container: container.map(str::to_string),
            key: Some(DefinitionKey::TopLevel {
                uri: uri.clone(),
                name,
                container: container.map(str::to_string),
            }),
            children: Vec::new(),
        });
    }
}

fn type_export(decl: &TypeDecl, uri: &Url, outer: Option<&str>, source: &str) -> Export {
    let name = decl.name.text.clone();
    let child_container = match outer {
        Some(outer) => format!("{outer}.{name}"),
        None => name.clone(),
    };
    let mut children = Vec::new();
    for param in &decl.params {
        children.push(Export {
            name: param.text.clone(),
            kind: SymbolKind::TypeParameter,
            detail: None,
            doc: None,
            selection_range: param.range,
            full_range: param.range,
            container: Some(child_container.clone()),
            key: Some(DefinitionKey::Local {
                uri: uri.clone(),
                def_start: param.range.start,
            }),
            children: Vec::new(),
        });
    }
    children.extend(item_exports(
        &decl.members,
        uri,
        Some(&child_container),
        source,
    ));
    if let ast::TypeKind::TagUnion { tags, .. } = &decl.backing.kind {
        for tag in tags {
            children.push(Export {
                name: tag.name.text.clone(),
                kind: SymbolKind::Tag,
                detail: None,
                doc: None,
                selection_range: tag.name.range,
                full_range: tag.range,
                container: Some(child_container.clone()),
                key: Some(DefinitionKey::Local {
                    uri: uri.clone(),
                    def_start: tag.name.range.start,
                }),
                children: Vec::new(),
            });
        }
    }
    Export {
        name,
        kind: SymbolKind::Type,
        detail: Some(slice(source, decl.backing.range)),
        doc: decl.doc.as_ref().map(|doc| doc.text.trim().to_string()),
        selection_range: decl.name.range,
        full_range: decl.range,
        container: outer.map(str::to_string),
        key: Some(DefinitionKey::TopLevel {
            uri: uri.clone(),
            name: decl.name.text.clone(),
            container: outer.map(str::to_string),
        }),
        children,
    }
}

fn definition_kind(definition: &ast::Definition, in_type: bool) -> SymbolKind {
    if in_type {
        SymbolKind::Method
    } else if definition.is_var {
        SymbolKind::Variable
    } else if definition.annotation.as_ref().is_some_and(is_function_type)
        || matches!(definition.value.kind, ExprKind::Lambda { .. })
    {
        SymbolKind::Function
    } else {
        SymbolKind::Constant
    }
}

fn is_function_type(ty: &ast::TypeExpr) -> bool {
    matches!(ty.kind, ast::TypeKind::Function { .. })
}

fn slice(source: &str, range: TextRange) -> String {
    source
        .get(range.start as usize..range.end as usize)
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_aliases_are_added_when_a_package_is_indexed_first() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("aLib")).expect("package dir");
        std::fs::write(root.join("aLib/main.roc"), "package [Widget] {}\n").expect("package root");
        std::fs::write(root.join("aLib/Widget.roc"), "module [make]\n\nmake = 1\n")
            .expect("module");
        std::fs::write(
            root.join("z.roc"),
            "app [main!] {\n\tlib: \"./aLib/main.roc\",\n\troc: \"nightly\",\n}\n\nmain! = |_| {}\n",
        )
        .expect("app");
        let workspace = WorkspaceIndex::build(root, &AnalysisConfig::default());
        assert!(
            workspace.package("lib").is_some(),
            "{:?}",
            workspace.packages
        );
        assert_eq!(workspace.package("lib").unwrap().shorthand, "lib");
        assert!(workspace.find_module(Some("lib"), "Widget").is_some());
    }

    #[test]
    fn path_dependency_root_file_keeps_its_nonstandard_filename() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("lib")).expect("package dir");
        std::fs::write(root.join("lib/Lib.roc"), "package [Widget] {}\n").expect("package root");
        std::fs::write(root.join("lib/Widget.roc"), "module [make]\n\nmake = 1\n").expect("module");
        std::fs::write(
            root.join("main.roc"),
            "app [main!] {\n\tlib: \"./lib/Lib.roc\",\n\troc: \"nightly\",\n}\n\nmain! = |_| {}\n",
        )
        .expect("app");
        let workspace = WorkspaceIndex::build(root, &AnalysisConfig::default());
        let package = workspace.package("lib").expect("package alias");
        assert_eq!(package.root_file, Some(root.join("lib/Lib.roc")));
        assert!(workspace.find_module(Some("lib"), "Widget").is_some());
    }

    #[test]
    fn platform_headers_are_indexed_under_their_directory_name_and_alias() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("pForm")).expect("platform dir");
        std::fs::write(
            root.join("pForm/main.roc"),
            "platform \"\"\n\trequires {} {}\n\tprovides {} {}\n",
        )
        .expect("platform root");
        std::fs::write(
            root.join("pForm/Stdout.roc"),
            "module [line]\n\nline = |_| {}\n",
        )
        .expect("platform module");
        std::fs::write(
            root.join("z.roc"),
            "app [main!] {\n\tpf: platform \"./pForm/main.roc\",\n\troc: \"nightly\",\n}\n\nmain! = |_| {}\n",
        )
        .expect("app");
        let workspace = WorkspaceIndex::build(root, &AnalysisConfig::default());
        assert!(
            workspace.package("pf").is_some(),
            "{:?}",
            workspace.packages
        );
        assert!(workspace.find_module(Some("pf"), "Stdout").is_some());
    }

    #[test]
    fn include_and_exclude_paths_shape_the_workspace_index() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("workspace");
        let external = temp.path().join("external");
        std::fs::create_dir_all(root.join("skip")).expect("skip dir");
        std::fs::create_dir_all(&external).expect("external dir");
        std::fs::write(root.join("main.roc"), "module [main]\n\nmain = 1\n").expect("main");
        std::fs::write(
            root.join("skip/Hidden.roc"),
            "module [hidden]\n\nhidden = 1\n",
        )
        .expect("hidden");
        std::fs::write(
            external.join("Included.roc"),
            "module [included]\n\nincluded = 1\n",
        )
        .expect("included");

        let config = AnalysisConfig {
            include_paths: vec!["../external".to_string()],
            exclude_paths: vec!["skip".to_string()],
            ..AnalysisConfig::default()
        };
        let workspace = WorkspaceIndex::build(&root, &config);

        assert!(workspace.find_module(None, "main").is_some());
        assert!(workspace.find_module(None, "Included").is_some());
        assert!(workspace.find_module(None, "Hidden").is_none());
    }
}

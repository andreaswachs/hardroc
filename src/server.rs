//! LSP server: protocol plumbing, document synchronization, diagnostics, and
//! dispatch into the feature modules.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    CallHierarchyServerCapability, CodeAction, CodeActionKind, CodeActionOptions, CodeActionParams,
    CodeActionProviderCapability, CodeActionResponse, CodeLens, CodeLensOptions, CodeLensParams,
    CompletionItem, CompletionOptions, CompletionParams, CompletionResponse, DiagnosticOptions,
    DiagnosticServerCapabilities, DidChangeConfigurationParams, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidChangeWatchedFilesRegistrationOptions,
    DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentDiagnosticParams, DocumentDiagnosticReport,
    DocumentDiagnosticReportResult, DocumentFormattingParams, DocumentHighlight,
    DocumentHighlightParams, DocumentLink, DocumentLinkOptions, DocumentLinkParams,
    DocumentRangeFormattingParams, DocumentSymbolParams, DocumentSymbolResponse,
    ExecuteCommandOptions, ExecuteCommandParams, FileSystemWatcher, FoldingRange,
    FoldingRangeParams, FoldingRangeProviderCapability, FullDocumentDiagnosticReport, GlobPattern,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, HoverProviderCapability,
    InitializeParams, InitializeResult, InitializedParams, InlayHint, InlayHintParams, Location,
    MessageType, OneOf, PositionEncodingKind, PrepareRenameResponse, ReferenceParams, Registration,
    RelatedFullDocumentDiagnosticReport, RenameParams, SaveOptions, SelectionRange,
    SelectionRangeParams, SelectionRangeProviderCapability, SemanticTokensFullOptions,
    SemanticTokensOptions, SemanticTokensParams, SemanticTokensRangeParams,
    SemanticTokensRangeResult, SemanticTokensResult, SemanticTokensServerCapabilities,
    ServerCapabilities, ServerInfo, SignatureHelp, SignatureHelpOptions, SignatureHelpParams,
    SymbolInformation, TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, TextEdit, Url, WatchKind, WorkDoneProgressOptions, WorkspaceEdit,
    WorkspaceSymbolParams,
};
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::request::{
    GotoDeclarationParams, GotoImplementationParams, GotoTypeDefinitionParams,
};
use tower_lsp::{Client, LanguageServer};

use crate::analysis::{AnalysisHost, AnalyzedFile};
use crate::compiler;
use crate::config::Config;
use crate::features::{actions, hierarchy, intelligence, navigation, structure, FeatureContext};
use crate::format;
use crate::text::{apply_change, SourceFile};

#[derive(Debug, Clone)]
struct DocumentState {
    text: String,
    version: i32,
}

pub struct Backend {
    client: Client,
    config: RwLock<Config>,
    host: RwLock<Option<Arc<AnalysisHost>>>,
    documents: Arc<RwLock<HashMap<Url, DocumentState>>>,
    compiler_diagnostics: Arc<RwLock<HashMap<Url, Vec<lsp_types::Diagnostic>>>>,
    check_generation: Arc<AtomicU64>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            config: RwLock::new(Config::default()),
            host: RwLock::new(None),
            documents: Arc::new(RwLock::new(HashMap::new())),
            compiler_diagnostics: Arc::new(RwLock::new(HashMap::new())),
            check_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    fn config(&self) -> Config {
        self.config
            .read()
            .map(|config| config.clone())
            .unwrap_or_default()
    }

    fn host(&self) -> Option<Arc<AnalysisHost>> {
        self.host
            .read()
            .ok()
            .and_then(|host| host.as_ref().map(Arc::clone))
    }

    fn document_text(&self, uri: &Url) -> Option<String> {
        self.documents
            .read()
            .ok()
            .and_then(|documents| documents.get(uri).map(|doc| doc.text.clone()))
    }

    fn analysis(&self, uri: &Url) -> Option<(Arc<AnalysisHost>, Arc<AnalyzedFile>)> {
        let host = self.host()?;
        let analysis = host.analyze(uri)?;
        Some((host, analysis))
    }

    fn any_analysis(&self, host: &Arc<AnalysisHost>) -> Option<Arc<AnalyzedFile>> {
        host.all_analyses().into_iter().next().or_else(|| {
            host.workspace()
                .modules
                .iter()
                .find_map(|module| host.analyze(&module.uri))
        })
    }

    async fn publish_analysis_diagnostics(&self, uri: &Url) {
        let config = self.config();
        let Some(host) = self.host() else {
            return;
        };
        let compiler = self
            .compiler_diagnostics
            .read()
            .map(|map| map.clone())
            .unwrap_or_default();
        if let Some(diagnostics) = diagnostics_for(&host, uri, &config, &compiler) {
            let version = self
                .documents
                .read()
                .ok()
                .and_then(|documents| documents.get(uri).map(|document| document.version));
            self.client
                .publish_diagnostics(uri.clone(), diagnostics, version)
                .await;
        }
    }

    fn schedule_check(&self, uri: Url, immediate: bool, live_buffers: bool) {
        let config = self.config();
        if !config.compiler_diagnostics {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        let overlays: Vec<(PathBuf, String)> = if live_buffers {
            self.documents
                .read()
                .map(|documents| {
                    documents
                        .iter()
                        .filter_map(|(uri, state)| {
                            uri.to_file_path()
                                .ok()
                                .map(|path| (path, state.text.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let generation = self
            .check_generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        let client = self.client.clone();
        let config = config.clone();
        let check_generation = Arc::clone(&self.check_generation);
        let compiler_diagnostics = self.compiler_diagnostics.clone();
        let open: Vec<Url> = self
            .documents
            .read()
            .map(|documents| documents.keys().cloned().collect())
            .unwrap_or_default();
        let documents = self.documents.clone();

        tokio::spawn(async move {
            if !immediate && config.check_debounce_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(config.check_debounce_ms))
                    .await;
            }
            if check_generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let Some(entry) = host.entry_for_uri(&uri) else {
                return;
            };
            let workspace_root = host
                .roots()
                .into_iter()
                .filter(|root| entry.starts_with(root))
                .max_by_key(|root| root.components().count())
                .or_else(|| entry.parent().map(|parent| parent.to_path_buf()))
                .unwrap_or_else(|| host.root().to_path_buf());
            let result = if overlays.is_empty() {
                compiler::run_check(&config.roc_path, &entry, config.check_timeout_secs).await
            } else {
                compiler::run_check_with_overlay(
                    &config.roc_path,
                    &entry,
                    config.check_timeout_secs,
                    &workspace_root,
                    &overlays,
                )
                .await
            };
            if check_generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let mut by_uri: HashMap<Url, Vec<lsp_types::Diagnostic>> = HashMap::new();
            match result {
                Ok(diagnostics) => {
                    for diagnostic in diagnostics {
                        by_uri
                            .entry(diagnostic.uri.clone())
                            .or_default()
                            .push(compiler::to_lsp(&diagnostic));
                    }
                }
                Err(error) => {
                    client
                        .log_message(MessageType::WARNING, format!("roc check: {error}"))
                        .await;
                    return;
                }
            }
            let relevant = |uri: &Url| {
                uri.to_file_path()
                    .ok()
                    .is_some_and(|path| host.is_workspace_path(&path))
            };
            let previous = compiler_diagnostics
                .read()
                .map(|map| map.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            if let Ok(mut map) = compiler_diagnostics.write() {
                map.clear();
                for (uri, diagnostics) in by_uri {
                    if relevant(&uri) {
                        map.insert(uri, diagnostics);
                    }
                }
            }
            let snapshot = compiler_diagnostics
                .read()
                .map(|map| map.clone())
                .unwrap_or_default();
            for uri in previous {
                if relevant(&uri) && !snapshot.contains_key(&uri) {
                    client.publish_diagnostics(uri, Vec::new(), None).await;
                }
            }
            for (uri, diagnostics) in &snapshot {
                if relevant(uri)
                    && !documents
                        .read()
                        .map(|documents| documents.contains_key(uri))
                        .unwrap_or(false)
                {
                    client
                        .publish_diagnostics(uri.clone(), diagnostics.clone(), None)
                        .await;
                }
            }
            for uri in open {
                if documents
                    .read()
                    .map(|documents| documents.contains_key(&uri))
                    .unwrap_or(false)
                {
                    if let Some(diagnostics) = diagnostics_for(&host, &uri, &config, &snapshot) {
                        client.publish_diagnostics(uri, diagnostics, None).await;
                    }
                }
            }
        });
    }

    fn feature_context<'a>(
        &'a self,
        host: &'a AnalysisHost,
        analysis: &'a AnalyzedFile,
        config: &'a Config,
    ) -> FeatureContext<'a> {
        FeatureContext::new(host, analysis, config)
    }

    fn goto_impl(
        &self,
        params: GotoDefinitionParams,
        kind: navigation::NavigationKind,
    ) -> Option<GotoDefinitionResponse> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let (host, analysis) = self.analysis(&uri)?;
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        navigation::goto(&ctx, &params, kind)
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        let mut config = Config::default();
        if let Some(options) = params.initialization_options.as_ref() {
            config.apply_json(options);
        }
        let mut roots = Vec::new();
        if let Some(folders) = &params.workspace_folders {
            for folder in folders {
                if let Ok(path) = folder.uri.to_file_path() {
                    roots.push(path);
                }
            }
        }
        if let Some(path) = params
            .root_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok())
        {
            roots.push(path);
        }
        if roots.is_empty() {
            roots.push(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        }
        let mut deduped = Vec::new();
        for root in roots {
            if !deduped.contains(&root) {
                deduped.push(root);
            }
        }
        if let Ok(mut stored) = self.config.write() {
            *stored = config.clone();
        }
        let host = Arc::new(AnalysisHost::new_multi(deduped, config.analysis_config()));
        if let Ok(mut stored) = self.host.write() {
            *stored = Some(host);
        }

        Ok(InitializeResult {
            capabilities: server_capabilities(),
            server_info: Some(ServerInfo {
                name: "hardroc".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(
                MessageType::INFO,
                format!("hardroc {} initialized", env!("CARGO_PKG_VERSION")),
            )
            .await;
        if let Some(host) = self.host() {
            let client = self.client.clone();
            let fallback_host = Arc::clone(&host);
            tokio::spawn(async move {
                let workspace = tokio::task::spawn_blocking(move || host.rebuild_workspace())
                    .await
                    .unwrap_or_else(|_| fallback_host.workspace());
                client
                    .log_message(
                        MessageType::INFO,
                        format!("hardroc indexed {} modules", workspace.modules.len()),
                    )
                    .await;
            });
        }

        let watcher = DidChangeWatchedFilesRegistrationOptions {
            watchers: vec![FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*.roc".to_string()),
                kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
            }],
        };
        let registration = Registration {
            id: "hardroc-watch-roc".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: serde_json::to_value(watcher).ok(),
        };
        let _ = self.client.register_capability(vec![registration]).await;
    }

    async fn shutdown(&self) -> RpcResult<()> {
        if let Ok(mut host) = self.host.write() {
            *host = None;
        }
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let text = params.text_document.text;
        let version = params.text_document.version;
        if let Some(host) = self.host() {
            host.set_document(uri.clone(), text.clone());
        }
        if let Ok(mut documents) = self.documents.write() {
            documents.insert(uri.clone(), DocumentState { text, version });
        }
        self.publish_analysis_diagnostics(&uri).await;
        // Opening a document is not typing: do not make the first compiler
        // check wait for the edit debounce. The overlay check still sees the
        // buffer text, so diagnostics can arrive before the first save.
        self.schedule_check(uri, true, true);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let mut text = self.document_text(&uri).unwrap_or_default();
        for change in params.content_changes {
            match change.range {
                Some(range) => {
                    let source = SourceFile::new(uri.clone(), text.clone());
                    apply_change(&mut text, &source, range, &change.text);
                }
                None => text = change.text,
            }
        }
        if let Some(host) = self.host() {
            host.set_document(uri.clone(), text.clone());
        }
        if let Ok(mut documents) = self.documents.write() {
            documents.insert(
                uri.clone(),
                DocumentState {
                    text,
                    version: params.text_document.version,
                },
            );
        }
        self.publish_analysis_diagnostics(&uri).await;
        self.schedule_check(uri, false, true);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let text = params.text.or_else(|| {
            uri.to_file_path()
                .ok()
                .and_then(|path| std::fs::read_to_string(path).ok())
        });
        if let Some(text) = text {
            if let Some(host) = self.host() {
                host.set_document(uri.clone(), text.clone());
            }
            if let Ok(mut documents) = self.documents.write() {
                if let Some(document) = documents.get_mut(&uri) {
                    document.text = text;
                }
            }
        }
        self.publish_analysis_diagnostics(&uri).await;
        self.schedule_check(uri, true, false);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        if let Some(host) = self.host() {
            host.remove_document(&uri);
        }
        if let Ok(mut documents) = self.documents.write() {
            documents.remove(&uri);
        }
        if let Ok(mut map) = self.compiler_diagnostics.write() {
            map.remove(&uri);
        }
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    async fn did_change_watched_files(&self, _params: DidChangeWatchedFilesParams) {
        if let Some(host) = self.host() {
            let client = self.client.clone();
            let fallback_host = Arc::clone(&host);
            let config = self.config();
            let compiler = self
                .compiler_diagnostics
                .read()
                .map(|map| map.clone())
                .unwrap_or_default();
            let open: Vec<Url> = self
                .documents
                .read()
                .map(|documents| documents.keys().cloned().collect())
                .unwrap_or_default();
            tokio::spawn(async move {
                let workspace = tokio::task::spawn_blocking(move || host.rebuild_workspace())
                    .await
                    .unwrap_or_else(|_| fallback_host.workspace());
                for uri in &open {
                    if let Some(diagnostics) =
                        diagnostics_for(&fallback_host, uri, &config, &compiler)
                    {
                        client
                            .publish_diagnostics(uri.clone(), diagnostics, None)
                            .await;
                    }
                }
                client
                    .log_message(
                        MessageType::INFO,
                        format!("hardroc reindexed {} modules", workspace.modules.len()),
                    )
                    .await;
            });
        }
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        let Some(host) = self.host() else {
            return;
        };
        let mut roots = host.roots();
        for folder in params.event.removed {
            if let Ok(path) = folder.uri.to_file_path() {
                roots.retain(|root| root != &path);
            }
        }
        for folder in params.event.added {
            if let Ok(path) = folder.uri.to_file_path() {
                if !roots.contains(&path) {
                    roots.push(path);
                }
            }
        }
        let fallback_host = Arc::clone(&host);
        let build_host = Arc::clone(&host);
        let workspace = tokio::task::spawn_blocking(move || build_host.set_roots(roots))
            .await
            .unwrap_or_else(|_| fallback_host.workspace());
        let config = self.config();
        let compiler = self
            .compiler_diagnostics
            .read()
            .map(|map| map.clone())
            .unwrap_or_default();
        let open: Vec<Url> = self
            .documents
            .read()
            .map(|documents| documents.keys().cloned().collect())
            .unwrap_or_default();
        for uri in open {
            if let Some(diagnostics) = diagnostics_for(&host, &uri, &config, &compiler) {
                self.client
                    .publish_diagnostics(uri, diagnostics, None)
                    .await;
            }
        }
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "hardroc indexed {} modules across {} workspace folders",
                    workspace.modules.len(),
                    host.roots().len()
                ),
            )
            .await;
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let mut config = self.config();
        config.apply_json(&params.settings);
        if !config.compiler_diagnostics {
            // Invalidate any in-flight `roc check` before clearing its cache so
            // a late result cannot resurrect disabled diagnostics.
            self.check_generation.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut map) = self.compiler_diagnostics.write() {
                map.clear();
            }
        }
        if let Ok(mut stored) = self.config.write() {
            *stored = config.clone();
        }
        if let Some(host) = self.host() {
            let workspace = host.update_config(config.analysis_config());
            let compiler = self
                .compiler_diagnostics
                .read()
                .map(|map| map.clone())
                .unwrap_or_default();
            let open: Vec<Url> = self
                .documents
                .read()
                .map(|documents| documents.keys().cloned().collect())
                .unwrap_or_default();
            for uri in open {
                if let Some(diagnostics) = diagnostics_for(&host, &uri, &config, &compiler) {
                    self.client
                        .publish_diagnostics(uri, diagnostics, None)
                        .await;
                }
            }
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "hardroc configuration updated; indexed {} modules",
                        workspace.modules.len()
                    ),
                )
                .await;
        }
    }

    async fn completion(&self, params: CompletionParams) -> RpcResult<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(intelligence::completion(&ctx, &params))
    }

    async fn completion_resolve(&self, params: CompletionItem) -> RpcResult<CompletionItem> {
        let Some(host) = self.host() else {
            return Ok(params);
        };
        let uri = params
            .data
            .as_ref()
            .and_then(|data| data.get("uri"))
            .and_then(|value| value.as_str())
            .and_then(|value| Url::parse(value).ok());
        let Some(analysis) = uri
            .as_ref()
            .and_then(|uri| host.analyze(uri))
            .or_else(|| self.any_analysis(&host))
        else {
            return Ok(params);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(intelligence::resolve_completion(&ctx, params))
    }

    async fn hover(&self, params: HoverParams) -> RpcResult<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(intelligence::hover(&ctx, &params))
    }

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> RpcResult<Option<SignatureHelp>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(intelligence::signature_help(&ctx, &params))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> RpcResult<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(intelligence::inlay_hints(&ctx, &params))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        Ok(self.goto_impl(params, navigation::NavigationKind::Definition))
    }

    async fn goto_declaration(
        &self,
        params: GotoDeclarationParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        Ok(self.goto_impl(
            GotoDefinitionParams {
                text_document_position_params: params.text_document_position_params,
                work_done_progress_params: params.work_done_progress_params,
                partial_result_params: params.partial_result_params,
            },
            navigation::NavigationKind::Declaration,
        ))
    }

    async fn goto_type_definition(
        &self,
        params: GotoTypeDefinitionParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        Ok(self.goto_impl(
            GotoDefinitionParams {
                text_document_position_params: params.text_document_position_params,
                work_done_progress_params: params.work_done_progress_params,
                partial_result_params: params.partial_result_params,
            },
            navigation::NavigationKind::TypeDefinition,
        ))
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        Ok(self.goto_impl(
            GotoDefinitionParams {
                text_document_position_params: params.text_document_position_params,
                work_done_progress_params: params.work_done_progress_params,
                partial_result_params: params.partial_result_params,
            },
            navigation::NavigationKind::Implementation,
        ))
    }

    async fn references(&self, params: ReferenceParams) -> RpcResult<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(Some(navigation::references(&ctx, &params)))
    }

    async fn prepare_rename(
        &self,
        params: lsp_types::TextDocumentPositionParams,
    ) -> RpcResult<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(navigation::prepare_rename(&ctx, &params))
    }

    async fn rename(&self, params: RenameParams) -> RpcResult<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(navigation::rename(&ctx, &params))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> RpcResult<Option<Vec<DocumentHighlight>>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(Some(navigation::highlights(&ctx, &params)))
    }

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> RpcResult<Option<Vec<CallHierarchyItem>>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(hierarchy::prepare(&ctx, &params))
    }

    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> RpcResult<Option<Vec<CallHierarchyIncomingCall>>> {
        let uri = params.item.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(Some(Vec::new()));
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(hierarchy::incoming(&ctx, &params).or(Some(Vec::new())))
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> RpcResult<Option<Vec<CallHierarchyOutgoingCall>>> {
        let uri = params.item.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(Some(Vec::new()));
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(hierarchy::outgoing(&ctx, &params).or(Some(Vec::new())))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> RpcResult<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(structure::document_symbols(&ctx, &params))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> RpcResult<Option<Vec<SymbolInformation>>> {
        let Some(host) = self.host() else {
            return Ok(None);
        };
        let Some(analysis) = self.any_analysis(&host) else {
            return Ok(Some(Vec::new()));
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(Some(structure::workspace_symbols(&ctx, &params)))
    }

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> RpcResult<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(Some(structure::folding_ranges(&ctx, &params)))
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> RpcResult<Option<Vec<SelectionRange>>> {
        let uri = params.text_document.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(Some(structure::selection_ranges(&ctx, &params)))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> RpcResult<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(Some(SemanticTokensResult::Tokens(
            structure::semantic_tokens(&ctx, &params),
        )))
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> RpcResult<Option<SemanticTokensRangeResult>> {
        let uri = params.text_document.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(Some(SemanticTokensRangeResult::Tokens(
            structure::semantic_tokens_range(&ctx, &params),
        )))
    }

    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> RpcResult<Option<Vec<DocumentLink>>> {
        let uri = params.text_document.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(Some(structure::document_links(&ctx, &params)))
    }

    async fn code_lens(&self, params: CodeLensParams) -> RpcResult<Option<Vec<CodeLens>>> {
        let uri = params.text_document.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(Some(structure::code_lenses(&ctx, &params)))
    }

    async fn code_action(&self, params: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        let uri = params.text_document.uri.clone();
        let Some((host, analysis)) = self.analysis(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(Some(actions::code_actions(&ctx, &params)))
    }

    async fn code_action_resolve(&self, params: CodeAction) -> RpcResult<CodeAction> {
        let Some(host) = self.host() else {
            return Ok(params);
        };
        let uri = params
            .data
            .as_ref()
            .and_then(|data| data.get("uri"))
            .and_then(|value| value.as_str())
            .and_then(|value| Url::parse(value).ok());
        let Some(analysis) = uri
            .as_ref()
            .and_then(|uri| host.analyze(uri))
            .or_else(|| self.any_analysis(&host))
        else {
            return Ok(params);
        };
        let config = self.config();
        let ctx = self.feature_context(&host, &analysis, &config);
        Ok(actions::resolve_code_action(&ctx, params))
    }

    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> RpcResult<DocumentDiagnosticReportResult> {
        let uri = params.text_document.uri;
        let config = self.config();
        let compiler = self
            .compiler_diagnostics
            .read()
            .map(|map| map.clone())
            .unwrap_or_default();
        let diagnostics = self
            .host()
            .and_then(|host| diagnostics_for(&host, &uri, &config, &compiler))
            .unwrap_or_default();
        Ok(DocumentDiagnosticReportResult::Report(
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                related_documents: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: None,
                    items: diagnostics,
                },
            }),
        ))
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> RpcResult<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri.clone();
        let Some(text) = self.document_text(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        match format::format_source(&config.roc_path, &text, config.format_timeout_secs).await {
            Ok(formatted) => {
                Ok(actions::full_document_edit(&text, &formatted).map(|edit| vec![edit]))
            }
            Err(error) => {
                self.client
                    .log_message(MessageType::ERROR, format!("roc fmt: {error}"))
                    .await;
                Ok(None)
            }
        }
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> RpcResult<Option<Vec<TextEdit>>> {
        // `roc fmt` formats whole modules. Formatting the document and returning
        // the single edit is correct even when the cursor has a selection; the
        // editor still gets canonical Roc formatting.
        let uri = params.text_document.uri.clone();
        let Some(text) = self.document_text(&uri) else {
            return Ok(None);
        };
        let config = self.config();
        match format::format_source(&config.roc_path, &text, config.format_timeout_secs).await {
            Ok(formatted) => Ok(actions::line_diff_edits(&text, &formatted, params.range)),
            Err(error) => {
                self.client
                    .log_message(MessageType::ERROR, format!("roc fmt: {error}"))
                    .await;
                Ok(None)
            }
        }
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> RpcResult<Option<serde_json::Value>> {
        let config = self.config();
        let root = self
            .host()
            .map(|host| host.workspace_root())
            .unwrap_or_else(|| crate::analysis::WorkspaceRoot::new(PathBuf::from(".")));
        let entry = root
            .entry
            .clone()
            .unwrap_or_else(|| root.root.join("main.roc"));
        match params.command.as_str() {
            "hardroc.version" => Ok(Some(serde_json::json!({
                "server": env!("CARGO_PKG_VERSION"),
                "builtins": self.host().map(|host| host.builtins().version().to_string()),
            }))),
            "hardroc.deps" => {
                let file = command_path_argument(&params.arguments, 0).unwrap_or(entry);
                let output = tokio::time::timeout(
                    std::time::Duration::from_secs(config.check_timeout_secs),
                    tokio::process::Command::new(&config.roc_path)
                        .arg("deps")
                        .arg(&file)
                        .output(),
                )
                .await;
                match output {
                    Ok(Ok(output)) => Ok(Some(serde_json::json!({
                        "success": output.status.success(),
                        "stdout": String::from_utf8_lossy(&output.stdout),
                        "stderr": String::from_utf8_lossy(&output.stderr),
                    }))),
                    Ok(Err(error)) => Ok(Some(serde_json::json!({ "error": error.to_string() }))),
                    Err(_) => Ok(Some(serde_json::json!({
                        "error": format!("roc deps timed out after {}s", config.check_timeout_secs)
                    }))),
                }
            }
            "hardroc.openPackageSource" => {
                let uri = command_uri_argument(&params.arguments, 0).ok_or_else(|| {
                    tower_lsp::jsonrpc::Error::invalid_params("expected a package source URI")
                })?;
                let request = lsp_types::ShowDocumentParams {
                    uri,
                    external: Some(false),
                    take_focus: Some(true),
                    selection: None,
                };
                match self
                    .client
                    .send_request::<lsp_types::request::ShowDocument>(request)
                    .await
                {
                    Ok(result) => Ok(Some(serde_json::json!({ "success": result.success }))),
                    Err(error) => Ok(Some(serde_json::json!({ "error": error.to_string() }))),
                }
            }
            "hardroc.runTest" => {
                let file = params
                    .arguments
                    .first()
                    .and_then(|value| value.as_str())
                    .and_then(|value| {
                        Url::parse(value)
                            .ok()
                            .and_then(|url| url.to_file_path().ok())
                            .or_else(|| Some(PathBuf::from(value)))
                    })
                    .unwrap_or(entry);
                let output = tokio::process::Command::new(&config.roc_path)
                    .arg("test")
                    .arg(&file)
                    .arg("--no-color")
                    .output()
                    .await;
                match output {
                    Ok(output) => Ok(Some(serde_json::json!({
                        "success": output.status.success(),
                        "stdout": String::from_utf8_lossy(&output.stdout),
                        "stderr": String::from_utf8_lossy(&output.stderr),
                    }))),
                    Err(error) => Ok(Some(serde_json::json!({ "error": error.to_string() }))),
                }
            }
            "hardroc.packageInfo" => {
                let host = self.host();
                Ok(Some(serde_json::json!({
                    "workspace": root.root,
                    "entry": root.entry,
                    "packages": host.map(|host| host.workspace().packages.clone()),
                })))
            }
            "hardroc.showReferences" => {
                let uri = params
                    .arguments
                    .first()
                    .and_then(|value| value.as_str())
                    .and_then(|value| {
                        Url::parse(value)
                            .ok()
                            .or_else(|| Url::from_file_path(value).ok())
                    });
                let position = params
                    .arguments
                    .get(1)
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok());
                let (Some(uri), Some(position)) = (uri, position) else {
                    return Err(tower_lsp::jsonrpc::Error::invalid_params(
                        "showReferences expects a document URI and a position",
                    ));
                };
                let host = self
                    .host()
                    .ok_or_else(tower_lsp::jsonrpc::Error::internal_error)?;
                let analysis = host.analyze(&uri).ok_or_else(|| {
                    tower_lsp::jsonrpc::Error::invalid_params("document is not analyzed")
                })?;
                let config = self.config();
                let ctx = FeatureContext::new(&host, &analysis, &config);
                let request = ReferenceParams {
                    text_document_position: lsp_types::TextDocumentPositionParams {
                        text_document: lsp_types::TextDocumentIdentifier { uri },
                        position,
                    },
                    context: lsp_types::ReferenceContext {
                        include_declaration: true,
                    },
                    work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                    partial_result_params: lsp_types::PartialResultParams::default(),
                };
                let locations = navigation::references(&ctx, &request);
                Ok(Some(serde_json::json!({ "locations": locations })))
            }
            other => Err(tower_lsp::jsonrpc::Error::invalid_params(format!(
                "unknown command: {other}"
            ))),
        }
    }
}

fn command_path_argument(arguments: &[serde_json::Value], index: usize) -> Option<PathBuf> {
    command_uri_argument(arguments, index).and_then(|uri| uri.to_file_path().ok())
}

fn command_uri_argument(arguments: &[serde_json::Value], index: usize) -> Option<Url> {
    let value = arguments.get(index)?.as_str()?;
    Url::parse(value)
        .ok()
        .or_else(|| Url::from_file_path(value).ok())
}

#[allow(clippy::too_many_arguments)]
fn diagnostics_for(
    host: &AnalysisHost,
    uri: &Url,
    config: &Config,
    compiler: &HashMap<Url, Vec<lsp_types::Diagnostic>>,
) -> Option<Vec<lsp_types::Diagnostic>> {
    let analysis = host.analyze(uri)?;
    let mut diagnostics = Vec::new();
    if config.syntax_diagnostics {
        for diagnostic in &analysis.parsed.diagnostics {
            diagnostics.push(lsp_types::Diagnostic {
                range: analysis.source.range(diagnostic.range),
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                code: Some(lsp_types::NumberOrString::String(
                    diagnostic.code.to_string(),
                )),
                code_description: None,
                source: Some("hardroc".to_string()),
                message: diagnostic.message.clone(),
                related_information: None,
                tags: None,
                data: None,
            });
        }
    }
    for diagnostic in &analysis.diagnostics {
        diagnostics.push(lsp_types::Diagnostic {
            range: analysis.source.range(diagnostic.range),
            severity: Some(match diagnostic.severity {
                crate::analysis::DiagnosticSeverity::Error => lsp_types::DiagnosticSeverity::ERROR,
                crate::analysis::DiagnosticSeverity::Warning => {
                    lsp_types::DiagnosticSeverity::WARNING
                }
                crate::analysis::DiagnosticSeverity::Information => {
                    lsp_types::DiagnosticSeverity::INFORMATION
                }
                crate::analysis::DiagnosticSeverity::Hint => lsp_types::DiagnosticSeverity::HINT,
            }),
            code: Some(lsp_types::NumberOrString::String(diagnostic.code.clone())),
            code_description: None,
            source: Some("hardroc".to_string()),
            message: diagnostic.message.clone(),
            related_information: None,
            tags: None,
            data: None,
        });
    }
    if config.compiler_diagnostics {
        if let Some(extra) = compiler.get(uri) {
            diagnostics.extend(extra.iter().cloned());
        }
    }
    let mut seen = std::collections::HashSet::new();
    diagnostics.retain(|diagnostic| {
        seen.insert((
            diagnostic.range.start.line,
            diagnostic.range.start.character,
            diagnostic.range.end.line,
            diagnostic.range.end.character,
            diagnostic.message.clone(),
        ))
    });
    Some(diagnostics)
}

fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF16),
        call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                will_save: None,
                will_save_wait_until: None,
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(true),
                })),
            },
        )),
        completion_provider: Some(CompletionOptions {
            resolve_provider: Some(true),
            trigger_characters: Some(vec![
                ".".to_string(),
                ":".to_string(),
                "$".to_string(),
                "\\".to_string(),
                "\"".to_string(),
                "(".to_string(),
                ",".to_string(),
            ]),
            all_commit_characters: None,
            work_done_progress_options: WorkDoneProgressOptions::default(),
            completion_item: None,
        }),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
            retrigger_characters: Some(vec![",".to_string()]),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        declaration_provider: Some(lsp_types::DeclarationCapability::Simple(true)),
        type_definition_provider: Some(lsp_types::TypeDefinitionProviderCapability::Simple(true)),
        implementation_provider: Some(lsp_types::ImplementationProviderCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![
                CodeActionKind::QUICKFIX,
                CodeActionKind::REFACTOR,
                CodeActionKind::SOURCE,
                CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
            ]),
            work_done_progress_options: WorkDoneProgressOptions::default(),
            resolve_provider: Some(true),
        })),
        code_lens_provider: Some(CodeLensOptions {
            resolve_provider: Some(false),
        }),
        document_formatting_provider: Some(OneOf::Left(true)),
        document_range_formatting_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Right(lsp_types::RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        document_link_provider: Some(DocumentLinkOptions {
            resolve_provider: Some(false),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: semantic_token_legend(),
                range: Some(true),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                work_done_progress_options: WorkDoneProgressOptions::default(),
            },
        )),
        inlay_hint_provider: Some(OneOf::Left(true)),
        diagnostic_provider: Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
            identifier: Some("hardroc".to_string()),
            inter_file_dependencies: true,
            workspace_diagnostics: false,
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![
                "hardroc.version".to_string(),
                "hardroc.deps".to_string(),
                "hardroc.runTest".to_string(),
                "hardroc.packageInfo".to_string(),
            ],
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        workspace: Some(lsp_types::WorkspaceServerCapabilities {
            workspace_folders: Some(lsp_types::WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(lsp_types::OneOf::Left(true)),
            }),
            file_operations: None,
        }),
        ..ServerCapabilities::default()
    }
}

fn semantic_token_legend() -> lsp_types::SemanticTokensLegend {
    lsp_types::SemanticTokensLegend {
        token_types: vec![
            lsp_types::SemanticTokenType::NAMESPACE,
            lsp_types::SemanticTokenType::TYPE,
            lsp_types::SemanticTokenType::CLASS,
            lsp_types::SemanticTokenType::ENUM,
            lsp_types::SemanticTokenType::INTERFACE,
            lsp_types::SemanticTokenType::STRUCT,
            lsp_types::SemanticTokenType::TYPE_PARAMETER,
            lsp_types::SemanticTokenType::PARAMETER,
            lsp_types::SemanticTokenType::VARIABLE,
            lsp_types::SemanticTokenType::PROPERTY,
            lsp_types::SemanticTokenType::ENUM_MEMBER,
            lsp_types::SemanticTokenType::FUNCTION,
            lsp_types::SemanticTokenType::METHOD,
            lsp_types::SemanticTokenType::MACRO,
            lsp_types::SemanticTokenType::KEYWORD,
            lsp_types::SemanticTokenType::MODIFIER,
            lsp_types::SemanticTokenType::COMMENT,
            lsp_types::SemanticTokenType::STRING,
            lsp_types::SemanticTokenType::NUMBER,
            lsp_types::SemanticTokenType::REGEXP,
            lsp_types::SemanticTokenType::OPERATOR,
            lsp_types::SemanticTokenType::DECORATOR,
        ],
        token_modifiers: vec![
            lsp_types::SemanticTokenModifier::DECLARATION,
            lsp_types::SemanticTokenModifier::DEFINITION,
            lsp_types::SemanticTokenModifier::READONLY,
            lsp_types::SemanticTokenModifier::STATIC,
            lsp_types::SemanticTokenModifier::DEPRECATED,
            lsp_types::SemanticTokenModifier::ABSTRACT,
            lsp_types::SemanticTokenModifier::ASYNC,
            lsp_types::SemanticTokenModifier::MODIFICATION,
            lsp_types::SemanticTokenModifier::DOCUMENTATION,
            lsp_types::SemanticTokenModifier::DEFAULT_LIBRARY,
        ],
    }
}

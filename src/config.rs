//! Server configuration, read from initialization options, workspace settings,
//! environment variables, and sensible defaults.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// Path to the `roc` executable used for formatting and checking.
    pub roc_path: String,
    /// Run `roc check` in the background and publish its diagnostics.
    pub compiler_diagnostics: bool,
    /// Publish parser diagnostics immediately on every edit.
    pub syntax_diagnostics: bool,
    /// Debounce before running `roc check` after an edit.
    pub check_debounce_ms: u64,
    /// Timeout for a single `roc check` invocation.
    pub check_timeout_secs: u64,
    /// Formatting timeout.
    pub format_timeout_secs: u64,
    /// Analyze package sources from Roc's package cache.
    pub inspect_packages: bool,
    /// Maximum workspace files to index.
    pub max_workspace_files: usize,
    /// Show inlay hints.
    pub inlay_hints: InlayHintConfig,
    /// Completion options.
    pub completion: CompletionConfig,
    /// Extra directories to scan for Roc modules, relative to the workspace.
    pub include_paths: Vec<String>,
    /// Exclude globs are not supported; these path prefixes are skipped.
    pub exclude_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InlayHintConfig {
    pub enabled: bool,
    pub parameter_names: bool,
    pub parameter_types: bool,
    pub return_types: bool,
    pub binding_types: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CompletionConfig {
    pub snippets: bool,
    pub builtins: bool,
    pub package_members: bool,
    /// Add `completionItem/resolve` documentation for large lists.
    pub resolve_documentation: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            roc_path: std::env::var("HARDROC_ROC").unwrap_or_else(|_| "roc".to_string()),
            compiler_diagnostics: true,
            syntax_diagnostics: true,
            check_debounce_ms: 800,
            check_timeout_secs: 90,
            format_timeout_secs: 15,
            inspect_packages: true,
            max_workspace_files: 20_000,
            inlay_hints: InlayHintConfig::default(),
            completion: CompletionConfig::default(),
            include_paths: Vec::new(),
            exclude_paths: vec![
                "target".to_string(),
                ".git".to_string(),
                "node_modules".to_string(),
            ],
        }
    }
}

impl Default for InlayHintConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            parameter_names: true,
            parameter_types: true,
            return_types: true,
            binding_types: true,
        }
    }
}

impl Default for CompletionConfig {
    fn default() -> Self {
        Self {
            snippets: true,
            builtins: true,
            package_members: true,
            resolve_documentation: true,
        }
    }
}

impl Config {
    /// Apply an `initializationOptions` / `didChangeConfiguration` JSON object.
    ///
    /// Only fields present in the object are changed. Editors commonly send a
    /// partial settings table, so replacing the whole configuration here would
    /// silently reset unrelated options to their defaults.
    pub fn apply_json(&mut self, value: &serde_json::Value) {
        let Some(object) = value.as_object() else {
            return;
        };
        self.apply_fields(object);
        if let Some(nested) = object
            .get("hardroc")
            .and_then(serde_json::Value::as_object)
        {
            self.apply_fields(nested);
        }
    }

    fn apply_fields(&mut self, object: &serde_json::Map<String, serde_json::Value>) {
        set_field(&mut self.roc_path, object, "rocPath");
        set_field(
            &mut self.compiler_diagnostics,
            object,
            "compilerDiagnostics",
        );
        set_field(&mut self.syntax_diagnostics, object, "syntaxDiagnostics");
        set_field(&mut self.check_debounce_ms, object, "checkDebounceMs");
        set_field(&mut self.check_timeout_secs, object, "checkTimeoutSecs");
        set_field(&mut self.format_timeout_secs, object, "formatTimeoutSecs");
        set_field(&mut self.inspect_packages, object, "inspectPackages");
        set_field(&mut self.max_workspace_files, object, "maxWorkspaceFiles");
        set_field(&mut self.include_paths, object, "includePaths");
        set_field(&mut self.exclude_paths, object, "excludePaths");

        if let Some(hints) = object
            .get("inlayHints")
            .and_then(serde_json::Value::as_object)
        {
            set_field(&mut self.inlay_hints.enabled, hints, "enabled");
            set_field(
                &mut self.inlay_hints.parameter_names,
                hints,
                "parameterNames",
            );
            set_field(
                &mut self.inlay_hints.parameter_types,
                hints,
                "parameterTypes",
            );
            set_field(&mut self.inlay_hints.return_types, hints, "returnTypes");
            set_field(&mut self.inlay_hints.binding_types, hints, "bindingTypes");
        }

        if let Some(completion) = object
            .get("completion")
            .and_then(serde_json::Value::as_object)
        {
            set_field(&mut self.completion.snippets, completion, "snippets");
            set_field(&mut self.completion.builtins, completion, "builtins");
            set_field(
                &mut self.completion.package_members,
                completion,
                "packageMembers",
            );
            set_field(
                &mut self.completion.resolve_documentation,
                completion,
                "resolveDocumentation",
            );
        }
    }

    pub fn analysis_config(&self) -> crate::analysis::AnalysisConfig {
        crate::analysis::AnalysisConfig {
            max_workspace_files: self.max_workspace_files,
            inspect_packages: self.inspect_packages,
            include_paths: self.include_paths.clone(),
            exclude_paths: self.exclude_paths.clone(),
        }
    }
}

fn set_field<T: serde::de::DeserializeOwned>(
    target: &mut T,
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) {
    if let Some(value) = object.get(key) {
        if let Ok(parsed) = serde_json::from_value(value.clone()) {
            *target = parsed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn partial_settings_preserve_unmentioned_options() {
        let mut config = Config {
            compiler_diagnostics: false,
            check_debounce_ms: 123,
            ..Config::default()
        };
        config.inlay_hints.parameter_names = false;

        config.apply_json(&json!({
            "checkTimeoutSecs": 42,
            "inlayHints": { "returnTypes": false }
        }));

        assert!(!config.compiler_diagnostics);
        assert_eq!(config.check_debounce_ms, 123);
        assert_eq!(config.check_timeout_secs, 42);
        assert!(!config.inlay_hints.parameter_names);
        assert!(!config.inlay_hints.return_types);
        assert!(config.inlay_hints.binding_types);
    }

    #[test]
    fn nested_hardroc_settings_merge_with_direct_settings() {
        let mut config = Config::default();
        config.apply_json(&json!({
            "formatTimeoutSecs": 9,
            "hardroc": {
                "compilerDiagnostics": false,
                "completion": { "snippets": false }
            }
        }));
        assert_eq!(config.format_timeout_secs, 9);
        assert!(!config.compiler_diagnostics);
        assert!(!config.completion.snippets);
        assert!(config.completion.builtins);
    }
}

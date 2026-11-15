//! Feature implementations, grouped by concern.
//!
//! Every function takes a [`FeatureContext`] (the current file's analysis plus
//! workspace access) and the LSP request parameters.  Keeping features out of
//! the server makes them directly testable and keeps the protocol layer boring.

pub mod actions;
pub mod hierarchy;
pub mod intelligence;
pub mod navigation;
pub mod structure;
pub mod util;

use crate::analysis::{AnalysisHost, AnalyzedFile};
use crate::config::Config;

pub struct FeatureContext<'a> {
    pub host: &'a AnalysisHost,
    pub analysis: &'a AnalyzedFile,
    pub config: &'a Config,
}

impl<'a> FeatureContext<'a> {
    pub fn new(host: &'a AnalysisHost, analysis: &'a AnalyzedFile, config: &'a Config) -> Self {
        Self {
            host,
            analysis,
            config,
        }
    }
}

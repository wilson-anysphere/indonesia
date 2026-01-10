use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct GeneratedSourcesConfig {
    /// Whether generated sources should be indexed and participate in resolution.
    pub enabled: bool,
    /// Additional generated roots (relative to project root unless absolute).
    pub additional_roots: Vec<PathBuf>,
    /// If set, replaces default discovery entirely.
    pub override_roots: Option<Vec<PathBuf>>,
}

impl Default for GeneratedSourcesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            additional_roots: Vec::new(),
            override_roots: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct NovaConfig {
    pub generated_sources: GeneratedSourcesConfig,
}


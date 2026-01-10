use nova_config::NovaConfig;
use nova_scheduler::CancellationToken;
use nova_types::ProjectId;
use std::sync::Arc;

#[derive(Clone)]
pub struct ExtensionContext<DB: ?Sized + Send + Sync> {
    pub db: Arc<DB>,
    pub config: Arc<NovaConfig>,
    pub project: ProjectId,
    pub cancel: CancellationToken,
}

impl<DB: ?Sized + Send + Sync> ExtensionContext<DB> {
    pub fn new(
        db: Arc<DB>,
        config: Arc<NovaConfig>,
        project: ProjectId,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            db,
            config,
            project,
            cancel,
        }
    }

    pub fn with_cancellation(&self, cancel: CancellationToken) -> Self {
        Self {
            db: Arc::clone(&self.db),
            config: Arc::clone(&self.config),
            project: self.project,
            cancel,
        }
    }
}

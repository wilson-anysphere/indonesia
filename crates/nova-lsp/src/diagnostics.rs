use std::{sync::Arc, time::Duration};

use lsp_types::{Diagnostic, Uri};
use nova_scheduler::{Cancelled, KeyedDebouncer, PoolKind, Scheduler};

#[derive(Clone)]
pub struct DiagnosticsDebouncer {
    debouncer: KeyedDebouncer<Uri>,
    publish: Arc<dyn Fn(Uri, Vec<Diagnostic>) + Send + Sync>,
}

impl DiagnosticsDebouncer {
    pub fn new(
        scheduler: Scheduler,
        publish: impl Fn(Uri, Vec<Diagnostic>) + Send + Sync + 'static,
    ) -> Self {
        Self::with_delay(scheduler, Scheduler::default_diagnostics_delay(), publish)
    }

    pub fn with_delay(
        scheduler: Scheduler,
        delay: Duration,
        publish: impl Fn(Uri, Vec<Diagnostic>) + Send + Sync + 'static,
    ) -> Self {
        Self {
            debouncer: KeyedDebouncer::new(scheduler, PoolKind::Compute, delay),
            publish: Arc::new(publish),
        }
    }

    pub fn schedule(&self, uri: Uri) {
        let publish = Arc::clone(&self.publish);
        self.debouncer.debounce(uri.clone(), move |token| {
            Cancelled::check(&token)?;
            publish(uri, Vec::new());
            Ok(())
        });
    }
}

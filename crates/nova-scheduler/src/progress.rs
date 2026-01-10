use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use tokio::sync::broadcast;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProgressId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    Begin {
        id: ProgressId,
        title: String,
        message: Option<String>,
        percentage: Option<u32>,
    },
    Report {
        id: ProgressId,
        message: Option<String>,
        percentage: Option<u32>,
    },
    End {
        id: ProgressId,
        message: Option<String>,
    },
}

pub type ProgressReceiver = broadcast::Receiver<ProgressEvent>;

#[derive(Clone)]
pub struct ProgressSender {
    tx: broadcast::Sender<ProgressEvent>,
    next_id: Arc<AtomicU64>,
}

impl ProgressSender {
    pub(crate) fn new(tx: broadcast::Sender<ProgressEvent>) -> Self {
        Self {
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn subscribe(&self) -> ProgressReceiver {
        self.tx.subscribe()
    }

    pub fn start(&self, title: impl Into<String>) -> Progress {
        let id = ProgressId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let title = title.into();
        let _ = self.tx.send(ProgressEvent::Begin {
            id,
            title,
            message: None,
            percentage: None,
        });
        Progress {
            id,
            tx: self.tx.clone(),
            finished: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[derive(Clone)]
pub struct Progress {
    id: ProgressId,
    tx: broadcast::Sender<ProgressEvent>,
    finished: Arc<AtomicBool>,
}

impl Progress {
    pub fn id(&self) -> ProgressId {
        self.id
    }

    pub fn report(&self, message: impl Into<Option<String>>, percentage: Option<u32>) {
        let _ = self.tx.send(ProgressEvent::Report {
            id: self.id,
            message: message.into(),
            percentage,
        });
    }

    pub fn finish(&self, message: impl Into<Option<String>>) {
        let message = message.into();
        if self
            .finished
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let _ = self.tx.send(ProgressEvent::End {
                id: self.id,
                message,
            });
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.finish(None);
    }
}

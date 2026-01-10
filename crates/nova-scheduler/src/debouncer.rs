use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use parking_lot::Mutex;

use crate::{CancellationToken, Cancelled, PoolKind, Scheduler};

struct DebounceEntry {
    id: u64,
    token: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

struct DebouncerInner<K> {
    scheduler: Scheduler,
    pool: PoolKind,
    delay: Duration,
    next_id: AtomicU64,
    entries: Mutex<HashMap<K, DebounceEntry>>,
}

#[derive(Clone)]
pub struct KeyedDebouncer<K> {
    inner: Arc<DebouncerInner<K>>,
}

pub struct KeyedDebouncedHandle {
    token: CancellationToken,
}

impl KeyedDebouncedHandle {
    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl<K> KeyedDebouncer<K>
where
    K: Clone + Eq + Hash + Send + 'static,
{
    pub fn new(scheduler: Scheduler, pool: PoolKind, delay: Duration) -> Self {
        Self {
            inner: Arc::new(DebouncerInner {
                scheduler,
                pool,
                delay,
                next_id: AtomicU64::new(1),
                entries: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn debounce<F>(&self, key: K, f: F) -> KeyedDebouncedHandle
    where
        F: FnOnce(CancellationToken) -> Result<(), Cancelled> + Send + 'static,
    {
        self.debounce_with_delay(key, self.inner.delay, f)
    }

    pub fn debounce_with_delay<F>(&self, key: K, delay: Duration, f: F) -> KeyedDebouncedHandle
    where
        F: FnOnce(CancellationToken) -> Result<(), Cancelled> + Send + 'static,
    {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let token = CancellationToken::new();

        if let Some(previous) = self.inner.entries.lock().remove(&key) {
            previous.token.cancel();
            previous.handle.abort();
        }

        let inner = Arc::clone(&self.inner);
        let key_for_task = key.clone();
        let token_for_task = token.clone();
        let mut f = Some(f);

        let handle = inner.scheduler.io_handle().spawn(async move {
            tokio::select! {
                _ = token_for_task.cancelled() => {}
                _ = tokio::time::sleep(delay) => {
                    if let Some(f) = f.take() {
                        let task = inner.scheduler.spawn_blocking_on(inner.pool, token_for_task.clone(), f);
                        let _ = task.join().await;
                    }
                }
            }

            let mut entries = inner.entries.lock();
            if let Some(current) = entries.get(&key_for_task) {
                if current.id == id {
                    entries.remove(&key_for_task);
                }
            }
        });

        self.inner.entries.lock().insert(
            key,
            DebounceEntry {
                id,
                token: token.clone(),
                handle,
            },
        );

        KeyedDebouncedHandle { token }
    }

    pub fn cancel(&self, key: &K) -> bool {
        let Some(entry) = self.inner.entries.lock().remove(key) else {
            return false;
        };
        entry.token.cancel();
        entry.handle.abort();
        true
    }
}

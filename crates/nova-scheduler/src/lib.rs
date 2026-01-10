use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// A generic per-category debounce helper.
///
/// Each category has its own debounce duration. Items are buffered until no new
/// items for that category have arrived within the configured window.
///
/// This is used by `nova-workspace` to debounce noisy filesystem watcher events
/// before kicking indexing / project reload work.
#[derive(Debug)]
pub struct Debouncer<C, T>
where
    C: Ord,
{
    windows: BTreeMap<C, DebounceWindow<T>>,
}

#[derive(Debug)]
struct DebounceWindow<T> {
    debounce: Duration,
    pending: Vec<T>,
    last_event_at: Option<Instant>,
}

impl<C, T> Debouncer<C, T>
where
    C: Ord + Clone,
{
    pub fn new(config: impl IntoIterator<Item = (C, Duration)>) -> Self {
        let mut windows = BTreeMap::new();
        for (category, debounce) in config {
            windows.insert(
                category,
                DebounceWindow {
                    debounce,
                    pending: Vec::new(),
                    last_event_at: None,
                },
            );
        }
        Self { windows }
    }

    /// Push an item into the debounce buffer for `category`.
    ///
    /// # Panics
    /// Panics if `category` was not configured when constructing the debouncer.
    pub fn push(&mut self, category: &C, item: T, now: Instant) {
        let window = self
            .windows
            .get_mut(category)
            .expect("debouncer category not configured");
        window.pending.push(item);
        window.last_event_at = Some(now);
    }

    /// Flush any categories that are due as of `now`.
    pub fn flush_due(&mut self, now: Instant) -> Vec<(C, Vec<T>)> {
        let mut flushed = Vec::new();
        for (category, window) in self.windows.iter_mut() {
            let Some(last) = window.last_event_at else {
                continue;
            };
            if now.duration_since(last) < window.debounce {
                continue;
            }
            if window.pending.is_empty() {
                window.last_event_at = None;
                continue;
            }
            flushed.push((category.clone(), std::mem::take(&mut window.pending)));
            window.last_event_at = None;
        }
        flushed
    }

    /// Returns the next deadline at which some category becomes flushable.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.windows
            .values()
            .filter_map(|w| w.last_event_at.map(|t| t + w.debounce))
            .min()
    }

    pub fn has_pending(&self) -> bool {
        self.windows.values().any(|w| !w.pending.is_empty())
    }

    /// Flush everything immediately (used on shutdown).
    pub fn flush_all(&mut self) -> Vec<(C, Vec<T>)> {
        let mut flushed = Vec::new();
        for (category, window) in self.windows.iter_mut() {
            if window.pending.is_empty() {
                window.last_event_at = None;
                continue;
            }
            flushed.push((category.clone(), std::mem::take(&mut window.pending)));
            window.last_event_at = None;
        }
        flushed
    }
}

/// Split `items` into chunks of at most `chunk_size`, preserving order.
///
/// # Panics
/// Panics if `chunk_size == 0`.
pub fn chunk_vec<T>(items: Vec<T>, chunk_size: usize) -> Vec<Vec<T>> {
    assert!(chunk_size > 0, "chunk_size must be > 0");
    let mut out = Vec::new();
    let mut iter = items.into_iter();
    loop {
        let mut chunk = Vec::with_capacity(chunk_size);
        for _ in 0..chunk_size {
            match iter.next() {
                Some(item) => chunk.push(item),
                None => break,
            }
        }
        if chunk.is_empty() {
            break;
        }
        out.push(chunk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq)]
    enum Cat {
        A,
        B,
    }

    #[test]
    fn debouncer_flushes_per_category() {
        let mut d = Debouncer::new([
            (Cat::A, Duration::from_millis(100)),
            (Cat::B, Duration::from_millis(200)),
        ]);

        let t0 = Instant::now();
        d.push(&Cat::A, 1, t0);
        d.push(&Cat::B, 10, t0);

        // Not due yet.
        assert!(d.flush_due(t0 + Duration::from_millis(50)).is_empty());

        // A becomes due first.
        let flushed = d.flush_due(t0 + Duration::from_millis(110));
        assert_eq!(flushed, vec![(Cat::A, vec![1])]);

        // B still pending.
        assert!(d.has_pending());

        let flushed = d.flush_due(t0 + Duration::from_millis(250));
        assert_eq!(flushed, vec![(Cat::B, vec![10])]);
        assert!(!d.has_pending());
    }

    #[test]
    fn chunk_vec_splits_preserving_order() {
        assert_eq!(chunk_vec(vec![1, 2, 3, 4, 5], 2), vec![vec![1, 2], vec![3, 4], vec![5]]);
        assert_eq!(chunk_vec(Vec::<u8>::new(), 3), Vec::<Vec<u8>>::new());
    }
}

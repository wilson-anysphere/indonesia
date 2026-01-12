use std::collections::{BTreeSet, HashMap, VecDeque};

use nova_jdwp::ObjectId;

/// Variables reference reserved for the synthetic "Pinned Objects" scope.
///
/// Keep this in the 32-bit signed range because many DAP clients parse
/// `variablesReference` as an `i32`.
pub const PINNED_SCOPE_REF: i64 = 0x7fff_ff00;

/// Offset applied to object handles when encoding them as DAP `variablesReference` values.
///
/// This avoids collisions with the small integers commonly used for scope roots (e.g. "Locals"
/// often uses `1`), while keeping the user-visible handle ID stable and small (`@1`, `@2`, ...).
pub const OBJECT_HANDLE_BASE: i64 = 1000;

/// Default maximum number of *unpinned* object handles kept in memory.
///
/// The registry provides stable handles for DAP `variablesReference` values.
/// Without bounding, long-lived debug sessions (or expanding large object graphs)
/// can grow memory usage without limit.
pub const DEFAULT_MAX_UNPINNED: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectHandle(u32);

impl ObjectHandle {
    pub fn from_variables_reference(variables_reference: i64) -> Option<Self> {
        if variables_reference <= OBJECT_HANDLE_BASE || variables_reference >= PINNED_SCOPE_REF {
            return None;
        }
        let raw = variables_reference - OBJECT_HANDLE_BASE;
        if raw <= 0 {
            return None;
        }
        Some(Self(raw as u32))
    }

    pub fn as_variables_reference(self) -> i64 {
        OBJECT_HANDLE_BASE + i64::from(self.0)
    }

    pub fn as_u32(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for ObjectHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "@{}", self.0)
    }
}

#[derive(Clone, Debug)]
struct ObjectEntry {
    object_id: ObjectId,
    runtime_type: String,
    evaluate_name: Option<String>,
    invalid: bool,
}

pub struct ObjectRegistry {
    next_handle: u32,
    object_to_handle: HashMap<ObjectId, ObjectHandle>,
    handle_to_entry: HashMap<ObjectHandle, ObjectEntry>,
    pinned: BTreeSet<ObjectHandle>,
    max_unpinned: usize,
    unpinned_fifo: VecDeque<ObjectHandle>,
}

impl ObjectRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_unpinned(max_unpinned: usize) -> Self {
        Self {
            max_unpinned,
            ..Self::default()
        }
    }

    pub fn max_unpinned(&self) -> usize {
        self.max_unpinned
    }

    /// Drop all tracked objects that are not currently pinned.
    ///
    /// This is intended for "stop-scoped" object handles: it bounds memory usage
    /// in long-lived debug sessions and makes it explicit that *unpinned* object
    /// handles are not guaranteed to survive a resume/next stop.
    pub fn clear_unpinned(&mut self) {
        if self.pinned.is_empty() {
            self.object_to_handle.clear();
            self.handle_to_entry.clear();
            self.unpinned_fifo.clear();
            return;
        }

        let mut object_to_handle = HashMap::with_capacity(self.pinned.len());
        let mut handle_to_entry = HashMap::with_capacity(self.pinned.len());

        for handle in self.pinned.iter().copied() {
            let Some(entry) = self.handle_to_entry.get(&handle).cloned() else {
                continue;
            };
            object_to_handle.insert(entry.object_id, handle);
            handle_to_entry.insert(handle, entry);
        }

        self.pinned
            .retain(|handle| handle_to_entry.contains_key(handle));
        self.object_to_handle = object_to_handle;
        self.handle_to_entry = handle_to_entry;
        self.unpinned_fifo.clear();
    }

    pub fn track_object(&mut self, object_id: ObjectId, runtime_type: &str) -> ObjectHandle {
        if let Some(handle) = self.object_to_handle.get(&object_id).copied() {
            if let Some(entry) = self.handle_to_entry.get_mut(&handle) {
                entry.runtime_type = runtime_type.to_string();
            }
            return handle;
        }

        let next = match self.next_handle {
            0 => 1,
            n => n,
        };

        // Prevent collisions with special scope references.
        assert!(
            OBJECT_HANDLE_BASE + i64::from(next) < PINNED_SCOPE_REF,
            "object handle space exhausted"
        );

        let handle = ObjectHandle(next);
        self.next_handle = next.saturating_add(1);
        self.object_to_handle.insert(object_id, handle);
        self.handle_to_entry.insert(
            handle,
            ObjectEntry {
                object_id,
                runtime_type: runtime_type.to_string(),
                evaluate_name: None,
                invalid: false,
            },
        );
        self.unpinned_fifo.push_back(handle);
        self.maybe_evict_unpinned();
        handle
    }

    pub fn object_id(&self, handle: ObjectHandle) -> Option<ObjectId> {
        self.handle_to_entry.get(&handle).map(|e| e.object_id)
    }

    pub fn runtime_type(&self, handle: ObjectHandle) -> Option<&str> {
        self.handle_to_entry
            .get(&handle)
            .map(|e| e.runtime_type.as_str())
    }

    pub fn set_evaluate_name(&mut self, handle: ObjectHandle, evaluate_name: String) {
        if let Some(entry) = self.handle_to_entry.get_mut(&handle) {
            entry.evaluate_name = Some(evaluate_name);
        }
    }

    pub fn evaluate_name(&self, handle: ObjectHandle) -> Option<&str> {
        self.handle_to_entry
            .get(&handle)
            .and_then(|e| e.evaluate_name.as_deref())
    }

    pub fn mark_invalid_object_id(&mut self, object_id: ObjectId) {
        if let Some(handle) = self.object_to_handle.get(&object_id).copied() {
            if let Some(entry) = self.handle_to_entry.get_mut(&handle) {
                entry.invalid = true;
            }
        }
    }

    pub fn is_invalid(&self, handle: ObjectHandle) -> bool {
        self.handle_to_entry
            .get(&handle)
            .map(|e| e.invalid)
            .unwrap_or(true)
    }

    pub fn pin(&mut self, handle: ObjectHandle) {
        if self.pinned.insert(handle) {
            // Remove from the unpinned eviction queue so we never evict a pinned handle.
            self.unpinned_fifo.retain(|h| *h != handle);
        }
    }

    pub fn unpin(&mut self, handle: ObjectHandle) {
        if self.pinned.remove(&handle) {
            // The object is now eligible for eviction, so enqueue it.
            if self.handle_to_entry.contains_key(&handle) {
                self.unpinned_fifo.push_back(handle);
                self.maybe_evict_unpinned();
            }
        }
    }

    pub fn is_pinned(&self, handle: ObjectHandle) -> bool {
        self.pinned.contains(&handle)
    }

    pub fn pinned_handles(&self) -> impl Iterator<Item = ObjectHandle> + '_ {
        self.pinned.iter().copied()
    }

    pub fn handle_for_object_id(&self, object_id: ObjectId) -> Option<ObjectHandle> {
        self.object_to_handle.get(&object_id).copied()
    }

    pub fn handle_from_variables_reference(
        &self,
        variables_reference: i64,
    ) -> Option<ObjectHandle> {
        let handle = ObjectHandle::from_variables_reference(variables_reference)?;
        self.handle_to_entry.contains_key(&handle).then_some(handle)
    }

    fn maybe_evict_unpinned(&mut self) {
        if self.unpinned_fifo.len() <= self.max_unpinned {
            return;
        }

        while self.unpinned_fifo.len() > self.max_unpinned {
            let Some(handle) = self.unpinned_fifo.pop_front() else {
                break;
            };
            // Paranoia: the queue should only contain unpinned handles, but avoid
            // evicting pinned handles if the bookkeeping ever gets out of sync.
            if self.pinned.contains(&handle) {
                continue;
            }

            let Some(entry) = self.handle_to_entry.remove(&handle) else {
                continue;
            };
            self.object_to_handle.remove(&entry.object_id);
        }
    }
}

impl Default for ObjectRegistry {
    fn default() -> Self {
        Self {
            next_handle: 0,
            object_to_handle: HashMap::new(),
            handle_to_entry: HashMap::new(),
            pinned: BTreeSet::new(),
            max_unpinned: DEFAULT_MAX_UNPINNED,
            unpinned_fifo: VecDeque::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_unpinned_handles_when_over_capacity() {
        let mut reg = ObjectRegistry::with_max_unpinned(2);
        let h1 = reg.track_object(1, "com.example.A");
        let h2 = reg.track_object(2, "com.example.A");
        let h3 = reg.track_object(3, "com.example.A");

        assert_eq!(reg.handle_for_object_id(1), None);
        assert_eq!(reg.handle_for_object_id(2), Some(h2));
        assert_eq!(reg.handle_for_object_id(3), Some(h3));
        // Ensure the original handle for the evicted object is no longer tracked.
        assert!(reg.object_id(h1).is_none());
    }

    #[test]
    fn pinned_handles_are_never_evicted() {
        let mut reg = ObjectRegistry::with_max_unpinned(1);
        let h1 = reg.track_object(1, "com.example.A");
        reg.pin(h1);

        let _h2 = reg.track_object(2, "com.example.A");
        let h3 = reg.track_object(3, "com.example.A");

        assert_eq!(reg.handle_for_object_id(1), Some(h1));
        assert!(reg.is_pinned(h1));
        assert_eq!(reg.handle_for_object_id(2), None);
        assert_eq!(reg.handle_for_object_id(3), Some(h3));
    }
}

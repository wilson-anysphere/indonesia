use std::collections::{BTreeSet, HashMap};

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
    invalid: bool,
}

#[derive(Default)]
pub struct ObjectRegistry {
    next_handle: u32,
    object_to_handle: HashMap<ObjectId, ObjectHandle>,
    handle_to_entry: HashMap<ObjectHandle, ObjectEntry>,
    pinned: BTreeSet<ObjectHandle>,
}

impl ObjectRegistry {
    pub fn new() -> Self {
        Self::default()
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
                invalid: false,
            },
        );
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
        self.pinned.insert(handle);
    }

    pub fn unpin(&mut self, handle: ObjectHandle) {
        self.pinned.remove(&handle);
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
}

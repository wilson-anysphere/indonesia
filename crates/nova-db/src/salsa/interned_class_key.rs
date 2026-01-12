use crate::ProjectId;

/// Prototype "interned key" used to explore class identity via `ra_ap_salsa`.
///
/// ## `ra_ap_salsa` API surface
///
/// `ra_ap_salsa` (as of `0.0.269`) does **not** expose a struct-level attribute
/// like `#[salsa::interned] struct Foo { .. }`. Instead, interning is expressed
/// as a *query* inside a `#[ra_salsa::query_group]` trait:
///
/// ```text
/// #[ra_salsa::interned]
/// fn intern_foo(&self, value: Foo) -> FooId;
/// ```
///
/// The `#[ra_salsa::query_group]` macro then auto-generates a companion
/// `lookup_intern_foo` query to map `FooId -> Foo`.
///
/// This file defines a minimal `InternedClassKey` value type plus an
/// `InternedClassKeyId` handle type, and tests how the resulting ids behave
/// under snapshots and `Database::evict_salsa_memos` (which rebuilds
/// `ra_salsa::Storage::default()`).
///
/// ## Raw id mapping to `nova_ids::ClassId`
///
/// Interned handles are thin wrappers around Salsa [`ra_salsa::InternId`], which
/// is a small (non-zero) `u32` newtype.
///
/// Nova's canonical strongly-typed `nova_ids::ClassId` is a transparent wrapper
/// around a `u32`, so converting between the two is straightforward:
///
/// ```rust
/// use nova_db::salsa::{InternedClassKey, InternedClassKeyId, NovaInternedClassKeys};
/// use nova_db::{ProjectId, SalsaDatabase};
/// use nova_ids::ClassId;
/// use ra_salsa::InternKey;
///
/// let db = SalsaDatabase::new();
/// let project = ProjectId::from_raw(0);
///
/// let key = InternedClassKey {
///     project,
///     name: "Foo".to_string(),
/// };
///
/// let interned: InternedClassKeyId = db.with_write(|db| db.intern_class_key(key));
///
/// // Persist the raw intern id as a `nova_ids::ClassId`.
/// let raw: u32 = interned.as_intern_id().as_u32();
/// let class_id = ClassId::from_raw(raw);
///
/// // Recover the interned handle from the raw id.
/// //
/// // SAFETY: `class_id` must have been produced by the same interner (same
/// // database storage) and must still refer to a live interned entry, or
/// // `lookup_intern_class_key` will panic.
/// let interned2 = unsafe { InternedClassKeyId::from_nova_class_id(class_id) };
/// assert_eq!(interned, interned2);
/// ```
///
/// ## Order dependence
///
/// `ra_ap_salsa` assigns intern ids densely as values are first seen. The raw
/// integer id is therefore **order dependent** across fresh databases:
/// interning `A` and then `B` yields different raw ids than interning `B` and
/// then `A`. This is important when evaluating whether interned ids can serve
/// as a globally stable `nova_ids::ClassId`.
///
/// See unit tests in this module (`interned_ids_depend_on_insertion_order_across_fresh_storages`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InternedClassKey {
    pub project: ProjectId,
    pub name: String,
}

// The interned query requires `Key: ra_salsa::InternValue`. We use the trivial
// mapping where the key is the value.
impl ra_salsa::InternValueTrivial for InternedClassKey {}

/// Handle returned by interning an [`InternedClassKey`].
///
/// This is the "identity" token we'd ultimately like to map onto
/// `nova_ids::ClassId`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct InternedClassKeyId(ra_salsa::InternId);

impl InternedClassKeyId {
    /// Convert this interned key into Nova's canonical `ClassId` wrapper.
    #[inline]
    pub fn to_nova_class_id(self) -> nova_ids::ClassId {
        nova_ids::ClassId::from_raw(self.0.as_u32())
    }

    /// Recreate an `InternedClassKeyId` from a stored [`nova_ids::ClassId`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that `id` was produced by
    /// [`InternedClassKeyId::to_nova_class_id`] for the *same* Salsa database
    /// storage, and that the interned entry is still present.
    ///
    /// In particular, Nova's `Database::evict_salsa_memos` rebuilds
    /// `ra_salsa::Storage::default()`, which drops intern tables. After an
    /// eviction, previously persisted ids may no longer refer to the same
    /// `InternedClassKey` (and lookups may panic).
    pub unsafe fn from_nova_class_id(id: nova_ids::ClassId) -> Self {
        Self(ra_salsa::InternId::from(id.to_raw()))
    }
}

impl ra_salsa::InternKey for InternedClassKeyId {
    fn from_intern_id(v: ra_salsa::InternId) -> Self {
        Self(v)
    }

    fn as_intern_id(&self) -> ra_salsa::InternId {
        self.0
    }
}

#[ra_salsa::query_group(NovaInternedClassKeysStorage)]
pub trait NovaInternedClassKeys: ra_salsa::Database {
    /// Intern `key` and return a compact identity handle.
    ///
    /// `#[ra_salsa::interned]` causes the query group macro to:
    /// - store the mapping in `ra_salsa::InternedStorage`
    /// - auto-generate `lookup_intern_class_key(id) -> InternedClassKey`
    #[ra_salsa::interned]
    fn intern_class_key(&self, key: InternedClassKey) -> InternedClassKeyId;
}

#[cfg(test)]
mod tests {
    use nova_memory::MemoryPressure;
    use ra_salsa::InternKey;

    use super::*;
    use crate::SalsaDatabase;

    #[test]
    fn interned_key_is_stable_within_a_single_storage() {
        let db = SalsaDatabase::new();
        let project = ProjectId::from_raw(0);
        let key = InternedClassKey {
            project,
            name: "Foo".to_string(),
        };

        let id1 = db.with_write(|db| db.intern_class_key(key.clone()));
        let id2 = db.with_write(|db| db.intern_class_key(key.clone()));

        assert_eq!(id1, id2);
        assert_eq!(id1.as_intern_id(), id2.as_intern_id());

        let looked_up = db.with_write(|db| db.lookup_intern_class_key(id1));
        assert_eq!(looked_up, key);
    }

    #[test]
    fn interned_key_is_consistent_across_snapshots() {
        let db = SalsaDatabase::new();
        let project = ProjectId::from_raw(0);
        let key = InternedClassKey {
            project,
            name: "Foo".to_string(),
        };

        let id = db.with_write(|db| db.intern_class_key(key.clone()));

        let snap = db.snapshot();
        let id_from_snapshot = snap.intern_class_key(key.clone());
        assert_eq!(id, id_from_snapshot);

        let looked_up = snap.lookup_intern_class_key(id_from_snapshot);
        assert_eq!(looked_up, key);
    }

    #[test]
    fn interned_ids_do_not_survive_salsa_memo_eviction() {
        let db = SalsaDatabase::new();
        let project = ProjectId::from_raw(0);

        // Intern a "sentinel" first so the key we care about does not get the
        // first intern id. If eviction rebuilds the intern tables, the next
        // `intern_class_key` will start assigning ids from the beginning, making
        // the change observable.
        let _sentinel = db.with_write(|db| {
            db.intern_class_key(InternedClassKey {
                project,
                name: "Sentinel".to_string(),
            })
        });

        let key = InternedClassKey {
            project,
            name: "Foo".to_string(),
        };
        let id_before = db.with_write(|db| db.intern_class_key(key.clone()));

        db.evict_salsa_memos(MemoryPressure::Critical);

        let id_after = db.with_write(|db| db.intern_class_key(key.clone()));

        // Observed behavior (ra_ap_salsa 0.0.269 + Nova's eviction strategy):
        // `evict_salsa_memos` rebuilds `ra_salsa::Storage::default()`, which
        // drops the intern tables. Interning the same key again yields a
        // *different* `InternId`.
        assert_ne!(id_before.as_intern_id(), id_after.as_intern_id());
    }

    #[test]
    fn interned_ids_depend_on_insertion_order_across_fresh_storages() {
        let project = ProjectId::from_raw(0);
        let a = InternedClassKey {
            project,
            name: "A".to_string(),
        };
        let b = InternedClassKey {
            project,
            name: "B".to_string(),
        };

        let db1 = SalsaDatabase::new();
        let a1 = db1.with_write(|db| db.intern_class_key(a.clone()));
        let b1 = db1.with_write(|db| db.intern_class_key(b.clone()));

        let db2 = SalsaDatabase::new();
        let b2 = db2.with_write(|db| db.intern_class_key(b.clone()));
        let a2 = db2.with_write(|db| db.intern_class_key(a.clone()));

        // `ra_ap_salsa` assigns intern ids densely in order of first insertion.
        // Therefore the raw ids depend on which key was interned first.
        assert_ne!(a1.as_intern_id(), a2.as_intern_id());
        assert_ne!(b1.as_intern_id(), b2.as_intern_id());
    }

    #[test]
    fn looking_up_a_pre_eviction_id_after_eviction_panics() {
        let db = SalsaDatabase::new();
        let project = ProjectId::from_raw(0);

        let _sentinel = db.with_write(|db| {
            db.intern_class_key(InternedClassKey {
                project,
                name: "Sentinel".to_string(),
            })
        });
        let id_before = db.with_write(|db| {
            db.intern_class_key(InternedClassKey {
                project,
                name: "Foo".to_string(),
            })
        });

        db.evict_salsa_memos(MemoryPressure::Critical);

        // After eviction the Salsa storage is rebuilt. The intern tables are
        // empty, so `id_before` no longer refers to a valid interned entry in
        // the fresh storage. The auto-generated lookup query will panic (index
        // out of bounds).
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.with_write(|db| db.lookup_intern_class_key(id_before))
        }));
        assert!(result.is_err());
    }
}

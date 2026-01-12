use std::collections::HashMap;

use nova_core::{ClassId, ProjectId};

/// Key used to allocate stable [`ClassId`]s across the lifetime of the process.
///
/// `binary_name` is expected to be the JVM binary name (e.g. `java.lang.String`,
/// `com.example.Outer$Inner`), but the interner treats it as an opaque string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClassKey {
    pub project: ProjectId,
    pub binary_name: String,
}

impl ClassKey {
    pub fn new(project: ProjectId, binary_name: impl Into<String>) -> Self {
        Self {
            project,
            binary_name: binary_name.into(),
        }
    }
}

/// Stable, process-lifetime [`ClassId`] allocation.
///
/// This intentionally lives outside Salsa storage so it is unaffected by
/// `Database::evict_salsa_memos` which rebuilds the Salsa storage from scratch.
///
/// ## Salsa purity / determinism caveat
///
/// This interner is **not** tracked by Salsa. If it is accessed from within
/// Salsa queries, the numeric `ClassId` assigned to a given [`ClassKey`] can
/// become dependent on evaluation order (which queries ran first, thread
/// scheduling, etc).
///
/// That violates Salsa's "purity rule" unless callers ensure interning happens
/// in a deterministic, input-driven order (e.g. by pre-seeding all known class
/// keys in a stable sort order before any query results are observed).
#[derive(Debug, Default)]
pub struct ClassIdInterner {
    by_key: HashMap<ClassKey, ClassId>,
    by_id: Vec<ClassKey>,
}

impl ClassIdInterner {
    /// Intern `key`, allocating a fresh dense [`ClassId`] if needed.
    ///
    /// IDs are assigned densely starting at 0 and are never reused.
    pub fn intern(&mut self, key: ClassKey) -> ClassId {
        if let Some(&id) = self.by_key.get(&key) {
            return id;
        }

        let raw = u32::try_from(self.by_id.len())
            .expect("ClassIdInterner exhausted: too many distinct classes");
        let id = ClassId::from_raw(raw);
        self.by_id.push(key.clone());
        self.by_key.insert(key, id);
        id
    }

    pub fn lookup_key(&self, id: ClassId) -> Option<&ClassKey> {
        self.by_id.get(id.to_raw() as usize)
    }

    pub fn lookup_id(&self, key: &ClassKey) -> Option<ClassId> {
        self.by_key.get(key).copied()
    }
}

/// Access to Nova's stable [`ClassIdInterner`].
pub trait HasClassInterner {
    fn class_interner(&self) -> &std::sync::Arc<parking_lot::Mutex<ClassIdInterner>>;

    fn intern_class_id(&self, project: ProjectId, binary_name: &str) -> ClassId {
        let mut interner = self.class_interner().lock();
        interner.intern(ClassKey::new(project, binary_name))
    }

    /// Reverse lookup: map an interned [`ClassId`] back to its [`ClassKey`].
    ///
    /// Note: this is intentionally named `lookup_*` to avoid colliding with
    /// `NovaResolve::class_key` (a Salsa query with a different meaning).
    fn lookup_class_key(&self, id: ClassId) -> Option<ClassKey> {
        let interner = self.class_interner().lock();
        interner.lookup_key(id).cloned()
    }

    /// Lookup an existing [`ClassId`] without allocating a new one.
    fn lookup_class_id(&self, key: &ClassKey) -> Option<ClassId> {
        let interner = self.class_interner().lock();
        interner.lookup_id(key)
    }
}

#[cfg(test)]
mod tests {
    use nova_memory::MemoryPressure;

    use super::*;
    use crate::salsa::{Database, RootDatabase};

    #[test]
    fn interner_is_idempotent_and_dense() {
        let mut interner = ClassIdInterner::default();
        let project = ProjectId::from_raw(0);

        let id1 = interner.intern(ClassKey::new(project, "java.lang.String"));
        let id2 = interner.intern(ClassKey::new(project, "java.lang.String"));
        assert_eq!(id1, id2);
        assert_eq!(id1.to_raw(), 0);

        let id3 = interner.intern(ClassKey::new(project, "java.lang.Object"));
        assert_eq!(id3.to_raw(), 1);
        assert_eq!(
            interner.lookup_key(id3).unwrap().binary_name,
            "java.lang.Object"
        );
    }

    #[test]
    fn interner_is_shared_across_snapshots() {
        let db = RootDatabase::default();
        let project = ProjectId::from_raw(0);

        let from_db = db.intern_class_id(project, "java.lang.String");
        let snap = db.snapshot();
        let from_snap = snap.intern_class_id(project, "java.lang.String");
        assert_eq!(from_db, from_snap);

        // Interning from a snapshot should update the shared process-lifetime interner.
        let from_snap_new = snap.intern_class_id(project, "java.lang.Object");
        let from_db_new = db.intern_class_id(project, "java.lang.Object");
        assert_eq!(from_snap_new, from_db_new);
    }

    #[test]
    fn interner_survives_salsa_memo_eviction() {
        let db = Database::new();
        let project = ProjectId::from_raw(0);

        let before = db.with_snapshot(|snap| snap.intern_class_id(project, "java.lang.String"));

        db.evict_salsa_memos(MemoryPressure::Critical);

        let after = db.with_snapshot(|snap| snap.intern_class_id(project, "java.lang.String"));
        assert_eq!(before, after);
    }

    #[test]
    fn interner_is_project_scoped() {
        let mut interner = ClassIdInterner::default();
        let name = "java.lang.String";

        let p0 = ProjectId::from_raw(0);
        let p1 = ProjectId::from_raw(1);

        let id0 = interner.intern(ClassKey::new(p0, name));
        let id1 = interner.intern(ClassKey::new(p1, name));

        assert_ne!(
            id0, id1,
            "expected different projects to allocate different ClassIds for the same binary name"
        );
        assert_eq!(
            interner.lookup_key(id0),
            Some(&ClassKey::new(p0, name)),
            "lookup_key should return the original project+name pair"
        );
        assert_eq!(
            interner.lookup_key(id1),
            Some(&ClassKey::new(p1, name)),
            "lookup_key should return the original project+name pair"
        );
    }

    #[test]
    fn interner_is_thread_safe_across_snapshots() {
        let db = RootDatabase::default();
        let project = ProjectId::from_raw(0);

        let snap1 = db.snapshot();
        let snap2 = db.snapshot();

        let h1 = std::thread::spawn(move || snap1.intern_class_id(project, "java.lang.String"));
        let h2 = std::thread::spawn(move || snap2.intern_class_id(project, "java.lang.String"));

        let id1 = h1.join().expect("thread 1 panicked");
        let id2 = h2.join().expect("thread 2 panicked");

        assert_eq!(id1, id2, "expected concurrent interning to be stable");
    }
}

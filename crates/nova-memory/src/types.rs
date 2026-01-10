use serde::{Deserialize, Serialize};

/// Coarse categories for Nova's memory budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    QueryCache,
    SyntaxTrees,
    Indexes,
    TypeInfo,
    Other,
}

/// Per-category memory breakdown in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBreakdown {
    pub query_cache: u64,
    pub syntax_trees: u64,
    pub indexes: u64,
    pub type_info: u64,
    pub other: u64,
}

impl MemoryBreakdown {
    pub fn total(self) -> u64 {
        self.query_cache + self.syntax_trees + self.indexes + self.type_info + self.other
    }

    pub fn get(self, category: MemoryCategory) -> u64 {
        match category {
            MemoryCategory::QueryCache => self.query_cache,
            MemoryCategory::SyntaxTrees => self.syntax_trees,
            MemoryCategory::Indexes => self.indexes,
            MemoryCategory::TypeInfo => self.type_info,
            MemoryCategory::Other => self.other,
        }
    }

    pub fn set(&mut self, category: MemoryCategory, bytes: u64) {
        match category {
            MemoryCategory::QueryCache => self.query_cache = bytes,
            MemoryCategory::SyntaxTrees => self.syntax_trees = bytes,
            MemoryCategory::Indexes => self.indexes = bytes,
            MemoryCategory::TypeInfo => self.type_info = bytes,
            MemoryCategory::Other => self.other = bytes,
        }
    }

    pub fn categories() -> [MemoryCategory; 5] {
        [
            MemoryCategory::QueryCache,
            MemoryCategory::SyntaxTrees,
            MemoryCategory::Indexes,
            MemoryCategory::TypeInfo,
            MemoryCategory::Other,
        ]
    }
}

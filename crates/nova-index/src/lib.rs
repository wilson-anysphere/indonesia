//! In-memory semantic indexes and their persistence integration.

mod indexes;
mod java_indexer;
mod java_types;
mod memory_cache;
mod persistence;
mod segments;
mod sketch;
mod symbol_search;
mod text_range;

pub use indexes::*;
pub use java_indexer::*;
pub use java_types::*;
pub use memory_cache::IndexCache;
pub use persistence::*;
pub use sketch::*;
pub use symbol_search::{
    CandidateStrategy, SearchResult, SearchStats, Symbol as SearchSymbol, SymbolSearchIndex,
    WorkspaceSymbolSearcher,
};
pub use text_range::TextRange;

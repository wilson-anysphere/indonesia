mod aquery_parser;
mod cache;
mod discovery;
mod workspace_aquery;
mod workspace_build;
mod workspace_cache;
mod workspace_cache_invalidation;
mod workspace_compile_info_for_file;
mod workspace_java_owners;
mod workspace_java_targets_universe;

#[cfg(feature = "bsp")]
mod bsp;
#[cfg(feature = "bsp")]
mod orchestrator;
#[cfg(feature = "bsp")]
mod workspace_bsp;
#[cfg(feature = "bsp")]
mod workspace_compile_info_for_file_bsp;
#[cfg(feature = "bsp")]
mod workspace_inverse_sources;


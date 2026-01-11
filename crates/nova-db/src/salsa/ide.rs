use std::time::Instant;

use crate::FileId;

use super::cancellation as cancel;
use super::semantic::NovaSemantic;
use super::stats::HasQueryStats;

#[ra_salsa::query_group(NovaIdeStorage)]
pub trait NovaIde: NovaSemantic + HasQueryStats {
    /// Debug query used to validate request cancellation behavior.
    ///
    /// Real queries (type-checking, indexing, etc.) should periodically call
    /// `db.unwind_if_cancelled()` while doing expensive work; this query exists
    /// as a lightweight fixture for that pattern.
    fn interruptible_work(&self, file: FileId, steps: u32) -> u64;
}

fn interruptible_work(db: &dyn NovaIde, file: FileId, steps: u32) -> u64 {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "interruptible_work", ?file, steps).entered();

    let mut acc: u64 = 0;
    for i in 0..steps {
        cancel::checkpoint_cancelled(db, i);
        acc = acc.wrapping_add(i as u64 ^ file.to_raw() as u64);
        std::hint::black_box(acc);
    }

    db.record_query_stat("interruptible_work", start.elapsed());
    acc
}

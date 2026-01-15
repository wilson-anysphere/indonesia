use std::time::Duration;

use nova_jdwp::{FrameId, JdwpClient};
use nova_stream_debug::{
    analyze_stream_expression, debug_stream, CancellationToken, StreamChain, StreamDebugConfig,
    StreamDebugError, StreamDebugResult,
};
use serde::{Deserialize, Serialize};

pub const STREAM_DEBUG_COMMAND: &str = "nova/streamDebug";

/// Hard cap on `StreamDebugConfig::max_sample_size` exposed via the DAP
/// `nova/streamDebug` request.
///
/// The stream debugger samples a bounded prefix of the stream. Keeping this capped avoids
/// expensive evaluations and large allocations when inspecting big streams.
pub const STREAM_DEBUG_MAX_SAMPLE_SIZE: usize = 25;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamDebugArguments {
    pub expression: String,
    #[serde(default)]
    pub frame_id: Option<i64>,
    #[serde(default)]
    pub max_sample_size: Option<usize>,
    #[serde(default)]
    pub max_total_time_ms: Option<u64>,
    #[serde(default)]
    pub allow_side_effects: bool,
    #[serde(default)]
    pub allow_terminal_ops: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamDebugBody {
    pub analysis: StreamChain,
    pub runtime: StreamDebugResult,
}

pub fn run_stream_debug<C: JdwpClient>(
    jdwp: &mut C,
    frame_id: FrameId,
    expression: &str,
    config: StreamDebugConfig,
) -> Result<StreamDebugBody, StreamDebugError> {
    let chain = analyze_stream_expression(expression)?;
    let cancel = CancellationToken::default();
    let runtime = debug_stream(jdwp, frame_id, &chain, &config, &cancel)?;
    Ok(StreamDebugBody {
        analysis: chain,
        runtime,
    })
}

impl StreamDebugArguments {
    pub fn into_config(&self) -> StreamDebugConfig {
        let mut cfg = StreamDebugConfig::default();
        if let Some(max) = self.max_sample_size {
            cfg.max_sample_size = max.min(STREAM_DEBUG_MAX_SAMPLE_SIZE);
        }
        if let Some(ms) = self.max_total_time_ms {
            cfg.max_total_time = Duration::from_millis(ms);
        }
        cfg.allow_side_effects = self.allow_side_effects;
        cfg.allow_terminal_ops = self.allow_terminal_ops;
        cfg.max_sample_size = cfg.max_sample_size.min(STREAM_DEBUG_MAX_SAMPLE_SIZE);
        cfg
    }
}

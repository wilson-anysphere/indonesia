#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::FuzzRunner;

use nova_remote_proto::v3::{
    self, Capabilities, CompressionAlgo, ProtocolVersion, SupportedVersions, WireFrame, WorkerHello,
};
use tokio::io::AsyncWriteExt as _;

struct State {
    rt: tokio::runtime::Runtime,
    hello_frame: Vec<u8>,
}

fn default_hello() -> WorkerHello {
    WorkerHello {
        shard_id: 0,
        auth_token: None,
        supported_versions: SupportedVersions {
            min: ProtocolVersion::CURRENT,
            max: ProtocolVersion::CURRENT,
        },
        // Keep negotiated limits small so the fuzz target can't allocate unbounded memory even if
        // the input contains hostile length prefixes.
        capabilities: Capabilities {
            max_frame_len: 64 * 1024,
            max_packet_len: 1024 * 1024,
            supported_compression: vec![CompressionAlgo::None],
            supports_cancel: true,
            supports_chunking: true,
        },
        cached_index_info: None,
        worker_build: None,
    }
}

fn init() -> State {
    let payload = v3::encode_wire_frame(&WireFrame::Hello(default_hello()))
        .expect("encode v3 hello wire frame");
    let len: u32 = payload
        .len()
        .try_into()
        .expect("v3 hello wire frame too large");

    let mut hello_frame = Vec::with_capacity(4 + payload.len());
    hello_frame.extend_from_slice(&len.to_le_bytes());
    hello_frame.extend_from_slice(&payload);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    State { rt, hello_frame }
}

fn run_one(state: &mut State, input: &[u8]) {
    state.rt.block_on(async {
        let (router_io, mut worker_io) = tokio::io::duplex(64 * 1024);

        let router_task = tokio::spawn(async move {
            nova_remote_rpc::RpcConnection::handshake_as_router(router_io, None).await
        });

        // Establish a valid handshake so the post-handshake read loop processes `data`.
        let _ = worker_io.write_all(&state.hello_frame).await;

        // Feed arbitrary bytes into the post-handshake framed transport.
        let _ = worker_io.write_all(input).await;
        let _ = worker_io.shutdown().await;

        // Avoid hanging if the handshake blocks for any reason (e.g. if the runtime changes).
        let _ = tokio::time::timeout(std::time::Duration::from_millis(50), router_task).await;

        // Give the spawned read loop a chance to process the buffered input before we drop the
        // runtime (which would abort outstanding tasks).
        tokio::task::yield_now().await;
    });
}

fn runner() -> &'static FuzzRunner<State> {
    static RUNNER: OnceLock<FuzzRunner<State>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new_default("v3_framed_transport", init, run_one))
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});

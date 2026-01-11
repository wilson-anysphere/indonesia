#![no_main]

use libfuzzer_sys::fuzz_target;

use nova_remote_proto::v3::{
    self, Capabilities, CompressionAlgo, ProtocolVersion, SupportedVersions, WireFrame, WorkerHello,
};
use tokio::io::AsyncWriteExt as _;

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

async fn write_wire_frame(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    frame: &WireFrame,
) -> Result<(), std::io::Error> {
    let payload = v3::encode_wire_frame(frame)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?;
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame too large"))?;
    stream.write_u32_le(len).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

fuzz_target!(|data: &[u8]| {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async move {
        let (router_io, mut worker_io) = tokio::io::duplex(64 * 1024);

        let router_task = tokio::spawn(async move {
            nova_remote_rpc::RpcConnection::handshake_as_router(router_io, None).await
        });

        // Establish a valid handshake so the post-handshake read loop processes `data`.
        let _ = write_wire_frame(&mut worker_io, &WireFrame::Hello(default_hello())).await;

        // Feed arbitrary bytes into the post-handshake framed transport.
        let _ = worker_io.write_all(data).await;
        let _ = worker_io.shutdown().await;

        // Avoid hanging if the handshake blocks for any reason (e.g. if the runtime changes).
        let _ = tokio::time::timeout(std::time::Duration::from_millis(50), router_task).await;

        // Give the spawned read loop a chance to process the buffered input before we drop the
        // runtime (which would abort outstanding tasks).
        tokio::task::yield_now().await;
    });
});


use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use nova_remote_proto::v3::{Capabilities, ProtocolVersion, SupportedVersions};
use nova_router::{
    DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, TcpListenAddr, WorkspaceLayout,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn router_handles_v3_hello_or_rejects_with_clear_error() -> Result<()> {
    let tmp = tempfile::tempdir().context("create temp dir")?;
    let cache_dir = tmp.path().join("cache");
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .context("create cache dir")?;

    let root = tmp.path().join("root");
    tokio::fs::create_dir_all(&root)
        .await
        .context("create source root")?;

    let addr = reserve_tcp_addr()?;
    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Tcp(TcpListenAddr::Plain(addr)),
        worker_command: PathBuf::from("unused"),
        cache_dir,
        auth_token: None,
        allow_insecure_tcp: false,
        max_rpc_bytes: nova_router::DEFAULT_MAX_RPC_BYTES,
        max_inflight_handshakes: nova_router::DEFAULT_MAX_INFLIGHT_HANDSHAKES,
        max_worker_connections: nova_router::DEFAULT_MAX_WORKER_CONNECTIONS,
        #[cfg(feature = "tls")]
        tls_client_cert_fingerprint_allowlist: Default::default(),
        spawn_workers: false,
    };
    let layout = WorkspaceLayout {
        source_roots: vec![SourceRoot { path: root }],
    };
    let router = QueryRouter::new_distributed(config, layout)
        .await
        .context("start distributed router")?;

    let mut stream = connect_with_retries(addr).await?;

    let hello = nova_remote_proto::v3::WireFrame::Hello(nova_remote_proto::v3::WorkerHello {
        shard_id: 0,
        auth_token: None,
        supported_versions: SupportedVersions {
            min: ProtocolVersion::CURRENT,
            max: ProtocolVersion::CURRENT,
        },
        capabilities: Capabilities::default(),
        cached_index_info: None,
        worker_build: None,
    });

    let payload = nova_remote_proto::v3::encode_wire_frame(&hello).context("encode v3 hello")?;
    write_len_prefixed(&mut stream, &payload).await?;

    let response = read_len_prefixed(&mut stream).await?;
    let frame =
        nova_remote_proto::v3::decode_wire_frame(&response).context("decode v3 reject frame")?;

    match frame {
        nova_remote_proto::v3::WireFrame::Reject(reject) => {
            assert_eq!(
                reject.code,
                nova_remote_proto::v3::RejectCode::UnsupportedVersion
            );
            assert!(
                reject.message.contains("legacy_v2"),
                "reject message should mention legacy_v2; got: {:?}",
                reject.message
            );
        }
        nova_remote_proto::v3::WireFrame::Welcome(welcome) => {
            // Post-migration routers should accept the v3 hello and respond with Welcome.
            assert_eq!(welcome.shard_id, 0);
        }
        other => return Err(anyhow!("expected v3 Welcome/Reject frame, got {other:?}")),
    }

    router.shutdown().await.context("shutdown router")?;
    Ok(())
}

fn reserve_tcp_addr() -> Result<SocketAddr> {
    let listener = StdTcpListener::bind("127.0.0.1:0").context("bind tcp listener")?;
    let addr = listener.local_addr().context("get local_addr")?;
    drop(listener);
    Ok(addr)
}

async fn connect_with_retries(addr: SocketAddr) -> Result<TcpStream> {
    let mut attempts = 0u32;
    loop {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(err) if attempts < 50 => {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                continue;
            }
            Err(err) => return Err(err).with_context(|| format!("connect to router {addr}")),
        }
    }
}

async fn write_len_prefixed(stream: &mut TcpStream, payload: &[u8]) -> Result<()> {
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("payload too large"))?;
    stream.write_u32_le(len).await.context("write len")?;
    stream.write_all(payload).await.context("write payload")?;
    stream.flush().await.context("flush payload")?;
    Ok(())
}

async fn read_len_prefixed(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let len = stream.read_u32_le().await.context("read len")?;
    let len_usize = len as usize;
    if len_usize > nova_remote_proto::MAX_MESSAGE_BYTES {
        return Err(anyhow!(
            "incoming frame too large: {len_usize} bytes (max {})",
            nova_remote_proto::MAX_MESSAGE_BYTES
        ));
    }
    let mut buf = vec![0u8; len_usize];
    stream.read_exact(&mut buf).await.context("read payload")?;
    Ok(buf)
}

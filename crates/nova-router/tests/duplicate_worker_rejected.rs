use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use nova_remote_proto::RpcMessage;
use nova_router::{
    DistributedRouterConfig, ListenAddr, QueryRouter, SourceRoot, TcpListenAddr, WorkspaceLayout,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_worker_connections_for_same_shard_are_rejected() -> Result<()> {
    let tmp = tempfile::tempdir().context("create temp dir")?;
    let root = tmp.path().join("root");
    tokio::fs::create_dir_all(&root)
        .await
        .context("create source root")?;

    let addr = reserve_tcp_addr()?;
    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Tcp(TcpListenAddr::Plain(addr)),
        worker_command: PathBuf::from("unused"),
        cache_dir: tmp.path().join("cache"),
        auth_token: None,
        allow_insecure_tcp: false,
        #[cfg(feature = "tls")]
        tls_client_cert_fingerprint_allowlist: Default::default(),
        spawn_workers: false,
    };
    let layout = WorkspaceLayout {
        source_roots: vec![SourceRoot { path: root }],
    };
    let router = QueryRouter::new_distributed(config, layout)
        .await
        .context("start router")?;

    let f1 = connect_and_hello(addr);
    let f2 = connect_and_hello(addr);
    let ((resp1, _stream1), (resp2, _stream2)) = tokio::try_join!(f1, f2)?;

    let responses = [resp1, resp2];
    let ok_count = responses
        .iter()
        .filter(|msg| matches!(msg, RpcMessage::RouterHello { .. }))
        .count();
    let err_messages: Vec<String> = responses
        .iter()
        .filter_map(|msg| match msg {
            RpcMessage::Error { message } => Some(message.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(
        ok_count, 1,
        "expected exactly one RouterHello, got responses: {responses:?}"
    );
    assert_eq!(
        err_messages.len(),
        1,
        "expected exactly one Error response, got: {responses:?}"
    );
    assert!(
        err_messages[0].contains("already has a connected worker"),
        "unexpected error message: {:?}",
        err_messages[0]
    );

    router.shutdown().await.context("shutdown router")?;
    Ok(())
}

async fn connect_and_hello(addr: SocketAddr) -> Result<(RpcMessage, TcpStream)> {
    let mut stream = connect_with_retries(addr).await?;
    write_message(
        &mut stream,
        &RpcMessage::WorkerHello {
            shard_id: 0,
            auth_token: None,
            has_cached_index: false,
        },
    )
    .await?;
    let resp = read_message(&mut stream).await?;
    Ok((resp, stream))
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

async fn write_message(stream: &mut TcpStream, message: &RpcMessage) -> Result<()> {
    let payload = nova_remote_proto::encode_message(message)?;
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("payload too large"))?;
    stream.write_u32_le(len).await.context("write len")?;
    stream.write_all(&payload).await.context("write payload")?;
    stream.flush().await.context("flush payload")?;
    Ok(())
}

async fn read_message(stream: &mut TcpStream) -> Result<RpcMessage> {
    let len = stream.read_u32_le().await.context("read len")?;
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await.context("read payload")?;
    Ok(nova_remote_proto::decode_message(&buf)?)
}

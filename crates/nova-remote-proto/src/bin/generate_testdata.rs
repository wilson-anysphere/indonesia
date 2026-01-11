use std::path::PathBuf;

use anyhow::Context;

use nova_remote_proto::transport;
use nova_remote_proto::RpcMessage;

fn main() -> anyhow::Result<()> {
    let message = RpcMessage::WorkerHello {
        shard_id: 1,
        auth_token: Some("test-token".into()),
        has_cached_index: true,
    };

    let bytes = transport::encode_framed_message(&message)?;
    let out_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/rpc_v2_hello.bin");
    std::fs::create_dir_all(out_path.parent().unwrap()).context("create testdata dir")?;
    std::fs::write(&out_path, &bytes).with_context(|| format!("write {}", out_path.display()))?;
    println!("wrote {} ({} bytes)", out_path.display(), bytes.len());
    Ok(())
}

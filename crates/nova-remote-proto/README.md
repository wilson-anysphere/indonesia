# `nova-remote-proto`

Wire format for Nova distributed / multi-process mode messaging (router ⇄ worker).

## Transport framing

On the wire, RPC messages are encoded as:

1. `u32` little-endian payload length
2. legacy `RpcMessage` payload (custom `legacy_v2` codec; intentionally not bincode)

Shared helpers live in `nova_remote_proto::transport`.

`MAX_FRAME_BYTES` caps the payload length prefix to prevent OOM from a hostile length.

You can further lower the framed transport limit at runtime by setting:

```bash
export NOVA_RPC_MAX_MESSAGE_SIZE=33554432  # 32 MiB
```

The value is read once (on first use) and clamped to `MAX_FRAME_BYTES`.

## Testing

```bash
cargo test -p nova-remote-proto
```

## Fuzzing

This crate includes a `cargo fuzz` harness in `crates/nova-remote-proto/fuzz/`.

```bash
cargo install cargo-fuzz
cd crates/nova-remote-proto
cargo fuzz run decode_framed_message
```

The `decode_framed_message` target feeds arbitrary bytes into
`transport::decode_framed_message` and asserts that decoding never panics and never allocates more
than `MAX_FRAME_BYTES` for a single frame.

## Golden vectors

`testdata/rpc_v2_hello.bin` is a known-good framed legacy `WorkerHello` for `PROTOCOL_VERSION == 4`.

To regenerate (e.g. after a deliberate protocol change and version bump):

```bash
cargo run -p nova-remote-proto --bin generate_testdata
```

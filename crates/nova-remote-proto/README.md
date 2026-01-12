# `nova-remote-proto`

Wire format for Nova distributed / multi-process mode messaging (router ⇄ worker).

This crate defines two protocol families:

- **v3 (current):** CBOR `WireFrame` envelopes + typed `Request`/`Response`/`Notification` payloads
  (`nova_remote_proto::v3`). The Tokio transport/runtime lives in `crates/nova-remote-rpc`.
  - On-the-wire spec: [`docs/17-remote-rpc-protocol.md`](../../docs/17-remote-rpc-protocol.md)
- **legacy_v2 (deprecated):** custom binary `legacy_v2::RpcMessage` codec kept for
  compatibility/tests (not wire-compatible with v3).

## Transport framing

Both protocols share the same outer framing:

1. `u32` little-endian payload length
2. payload bytes

Payload interpretation:

- v3: CBOR `v3::WireFrame`
- legacy_v2: `legacy_v2::RpcMessage` (custom codec; intentionally not bincode)

Shared helpers for the **legacy** framed transport live in `nova_remote_proto::transport`.

## Frame limits

Hard safety limits are enforced during decoding to avoid OOM from hostile inputs:

- `MAX_MESSAGE_BYTES` / `MAX_FRAME_BYTES` (currently 64 MiB)

The legacy framed transport (`nova_remote_proto::transport`) also supports lowering the effective
max frame size at runtime via:

```bash
export NOVA_RPC_MAX_MESSAGE_SIZE=33554432  # 32 MiB
```

The default framed transport limit is 32 MiB. The value is read once (on first use) and clamped to
`MAX_FRAME_BYTES`.

Note: v3 uses negotiated `max_frame_len` / `max_packet_len` and does not read this env var.

## Testing

```bash
bash scripts/cargo_agent.sh test --locked -p nova-remote-proto
```

## Fuzzing

This crate includes a `cargo fuzz` harness in `crates/nova-remote-proto/fuzz/`.

```bash
# cargo-fuzz requires nightly Rust + LLVM tools for libFuzzer integration.
rustup toolchain install nightly --component llvm-tools-preview --component rust-src

# Recommended (fast): install the prebuilt cargo-fuzz binary via cargo-binstall.
cargo install cargo-binstall --locked
cargo +nightly binstall cargo-fuzz --version 0.13.1 --no-confirm --locked --disable-strategies compile --disable-telemetry

cd crates/nova-remote-proto
cargo +nightly fuzz run decode_framed_message -- -max_total_time=60 -max_len=262144
cargo +nightly fuzz run decode_v3_wire_frame -- -max_total_time=60 -max_len=262144
cargo +nightly fuzz run decode_v3_rpc_payload -- -max_total_time=60 -max_len=262144
```

Targets:

- `decode_framed_message`: legacy framed transport (`transport::decode_framed_message`)
- `decode_v3_wire_frame`: v3 CBOR envelope decoding (`v3::decode_wire_frame`)
- `decode_v3_rpc_payload`: v3 application payload decoding (`v3::decode_rpc_payload`)

For end-to-end framed transport fuzzing (handshake + post-handshake framing), see the v3 transport
crate: `crates/nova-remote-rpc/fuzz/`.

## Golden vectors

`testdata/rpc_v2_hello.bin` is a known-good framed legacy `WorkerHello` for `PROTOCOL_VERSION == 4`.

To regenerate (e.g. after a deliberate protocol change and version bump):

```bash
bash scripts/cargo_agent.sh run --locked -p nova-remote-proto --bin generate_testdata
```

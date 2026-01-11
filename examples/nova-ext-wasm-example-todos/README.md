# `nova-ext-wasm-example-todos`

Minimal example Nova WASM extension that implements the **diagnostics** capability by flagging
`TODO` occurrences.

## Build

```bash
rustup target add wasm32-unknown-unknown
cargo build -p nova-ext-wasm-example-todos --release --target wasm32-unknown-unknown
```

The resulting module is:

```text
target/wasm32-unknown-unknown/release/nova_ext_wasm_example_todos.wasm
```

## Bundle layout

See `bundle/nova-ext.toml` for an example extension manifest.


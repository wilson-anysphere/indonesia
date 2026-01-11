# Fuzzing Nova

Nova includes a `cargo-fuzz` harness under `fuzz/` to ensure that core text-processing
pipelines (parser / formatter / refactorings) never panic or hang on malformed input.

The fuzz targets are intentionally **not** part of the main Cargo workspace, so normal
`cargo test` and CI remain unchanged unless you explicitly run fuzzing commands.

## Prerequisites

1. Install a nightly toolchain with LLVM tools:

```bash
rustup toolchain install nightly --component llvm-tools-preview
```

2. Install `cargo-fuzz`:

```bash
cargo +nightly install cargo-fuzz --locked
```

## Running fuzz targets

All commands below are run from the repository root.

### Parse Java (syntax)

```bash
cargo +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60
```

This target runs both `nova_syntax::parse` and `nova_syntax::parse_java` on the input.

### Format Java

```bash
cargo +nightly fuzz run fuzz_format -- -max_total_time=60
```

This target exercises `nova_format::format_java` and edit generation (`edits_for_formatting`).

### Parse JVM classfiles

```bash
cargo +nightly fuzz run fuzz_classfile -- -max_total_time=60
```

This target feeds arbitrary bytes into `nova_classfile::ClassFile::parse`.

### Optional targets

Nova also has additional fuzz targets for deeper invariants / higher-level smoke tests:

```bash
cargo +nightly fuzz run parse_java -- -max_total_time=60
cargo +nightly fuzz run format_java -- -max_total_time=60
cargo +nightly fuzz run refactor_smoke -- -max_total_time=60
```

- `format_java` asserts formatter idempotence (`format(format(x)) == format(x)`).
- `refactor_smoke` treats refactoring errors as expected and ignored; the target only enforces that
  Nova never panics or hangs while attempting a small set of best-effort refactorings.

Seed corpora live under `fuzz/corpus/<target>/`.

## Hangs, timeouts, and input size caps

Each fuzz target:

- caps the input to **256KiB** (to avoid OOM and pathological worst-case behavior)
- enforces a per-input wall-clock timeout and treats timeouts as fuzz failures (hangs)

## Crash artifacts and minimization

When a failure is found, libFuzzer writes the triggering input to:

`fuzz/artifacts/<target>/`

### Reproducing a failure

```bash
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<artifact>
```

### Minimizing a crash input

```bash
cargo +nightly fuzz tmin <target> fuzz/artifacts/<target>/<artifact>
```

### Minimizing a corpus (optional)

If you have a large local corpus under `fuzz/corpus/<target>/`, you can shrink it:

```bash
cargo +nightly fuzz cmin <target>
```

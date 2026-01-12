# Fuzzing Nova

Nova includes a `cargo-fuzz` harness under `fuzz/` to ensure that core parsing, formatting, and related
text-processing/config-parsing pipelines (parser / incremental reparsing / formatter / config parsing /
refactorings) never panic or hang on malformed input.

The fuzz targets are intentionally **not** part of the main Cargo workspace, so normal
`cargo test` and CI remain unchanged unless you explicitly run fuzzing commands.

## Prerequisites

1. Install a nightly toolchain with LLVM tools:

```bash
rustup toolchain install nightly --component llvm-tools-preview --component rust-src
```

2. Install `cargo-fuzz`:

```bash
# Recommended (fast): install the prebuilt cargo-fuzz binary via cargo-binstall.
cargo install cargo-binstall --locked
cargo +nightly binstall cargo-fuzz --version 0.13.1 --no-confirm --locked --disable-strategies compile --disable-telemetry

# Agent / multi-runner equivalent:
# bash scripts/cargo_agent.sh install cargo-binstall --locked
# bash scripts/cargo_agent.sh +nightly binstall cargo-fuzz --version 0.13.1 --no-confirm --locked --disable-strategies compile --disable-telemetry

# Alternative (slower): build cargo-fuzz from source.
# cargo +nightly install cargo-fuzz --version 0.13.1 --locked
```

## Running fuzz targets

Unless otherwise noted, commands below are run from the repository root.

## Agent / multi-runner environments (wrapper script)

If you're running fuzzing on a shared host with multiple concurrent agents, follow
[`AGENTS.md`](../AGENTS.md) and run Cargo via the wrapper script:

```bash
# Workstation / CI:
cargo +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60 -max_len=262144

# Agent / multi-runner:
bash scripts/cargo_agent.sh +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60 -max_len=262144
```

When running from a crate directory (for per-crate fuzz harnesses under `crates/*/fuzz/`), invoke
the wrapper relative to the repo root:

```bash
cd crates/nova-remote-proto
bash ../../scripts/cargo_agent.sh +nightly fuzz run decode_framed_message -- -max_total_time=60 -max_len=262144
```

> Note: the first `cargo fuzz` run can take a while because the toolchain builds the Rust standard
> library with the selected fuzzing settings. Subsequent runs reuse `fuzz/target/` and are much
> faster.
>
> If you see `Blocking waiting for file lock on ...`, another Cargo process is likely building at
> the same time. Either wait, or use a separate target directory (avoids contention on build
> artifacts):
>
> ```bash
> cargo +nightly fuzz run --target-dir fuzz/target-local fuzz_syntax_parse -- -max_total_time=60 -max_len=262144
> ```
>
> If the lock is on the *package cache*, you can also use a separate `CARGO_HOME`:
>
> ```bash
> CARGO_HOME=/tmp/nova-cargo-home cargo +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60 -max_len=262144
> ```

### Parse Java (syntax)

```bash
cargo +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60 -max_len=262144
```

This target runs both `nova_syntax::parse` and `nova_syntax::parse_java` on the input.

### Parse Java literals

```bash
cargo +nightly fuzz run fuzz_syntax_literals -- -max_total_time=60 -max_len=262144
```

This target exercises literal parsing and unescaping utilities (`nova_syntax::parse_*_literal`,
`unescape_*`) and enforces:

- never panic / never hang
- reported error spans always stay within the input bounds

### Reparse Java (incremental)

```bash
cargo +nightly fuzz run fuzz_reparse_java -- -max_total_time=60 -max_len=262144
```

This target exercises incremental reparsing (`nova_syntax::reparse_java`) by applying random text edits and
asserting that:

- the resulting syntax tree is lossless (roundtrips back to the edited text)
- incremental diagnostics match a full reparse’s diagnostics

### Reparse Java (incremental sequence)

```bash
cargo +nightly fuzz run fuzz_reparse_java_sequence -- -max_total_time=60 -max_len=262144
```

This target applies a small sequence of edits and asserts that incremental reparsing is equivalent to a
full reparse after each edit.

### Format Java

```bash
cargo +nightly fuzz run fuzz_format -- -max_total_time=60 -max_len=262144
```

This target exercises Nova's **full-document formatting** pipeline (the same entrypoint used by the
CLI + LSP): it generates formatting edits via
`nova_format::edits_for_document_formatting_with_strategy`, applies them with
`nova_core::apply_text_edits`, and checks that the edit pipeline is **idempotent** (formatting a
formatted document yields no further changes) for the production + legacy strategies.

### Format Java (range formatting)

```bash
cargo +nightly fuzz run fuzz_range_format -- -max_total_time=60 -max_len=262144
```

This target exercises range formatting (`nova_format::edits_for_range_formatting`) and enforces that any
returned edits stay within the selected range.

### Format Java (on-type formatting)

```bash
cargo +nightly fuzz run fuzz_on_type_format -- -max_total_time=60 -max_len=262144
```

This target exercises on-type formatting (`nova_format::edits_for_on_type_formatting`) with a few common
trigger characters (e.g. `;`, `}`, `)`).

### Java completions (code intelligence)

```bash
cargo +nightly fuzz run fuzz_completion -- -max_total_time=60 -max_len=262144
```

This target decodes arbitrary bytes to UTF-8 lossily and calls
`nova_ide::code_intelligence::completions` at a cursor position derived from the input.

### Parse JVM classfiles

```bash
cargo +nightly fuzz run fuzz_classfile -- -max_total_time=60 -max_len=262144
```

This target feeds arbitrary bytes into `nova_classfile::ClassFile::parse`.

### Decompile JVM classfiles (classfile-to-source fallback)

```bash
cargo +nightly fuzz run fuzz_decompile_classfile -- -max_total_time=60 -max_len=262144
```

This target feeds arbitrary bytes into `nova_decompile::decompile_classfile` and (when
decompilation succeeds) asserts that all `SymbolRange` mappings are in-bounds and convertible back
to byte ranges via `nova_core::LineIndex`.

### Parse JUnit XML reports

```bash
cargo +nightly fuzz run fuzz_junit_report -- -max_total_time=60 -max_len=262144
```

This target feeds arbitrary UTF-8 input into `nova_testing::report::parse_junit_report_str` and
treats parse errors as expected (the target only enforces "never panic / never hang").

### Parse YAML

```bash
cargo +nightly fuzz run fuzz_yaml_parse -- -max_total_time=60 -max_len=262144
```

This target exercises Nova’s YAML parser (`nova_yaml::parse`) and asserts that any returned key/value
ranges are safe to slice in the original UTF-8 input.

### Parse Java `.properties`

```bash
cargo +nightly fuzz run fuzz_properties_parse -- -max_total_time=60 -max_len=262144
```

This target exercises Nova’s `.properties` parser (`nova_properties::parse`) and asserts that any returned
key/value ranges are safe to slice in the original UTF-8 input.

### Parse Spring configuration metadata

```bash
cargo +nightly fuzz run fuzz_config_metadata -- -max_total_time=60 -max_len=262144
```

This target ingests arbitrary bytes into the Spring configuration metadata index
(`nova_config_metadata::MetadataIndex::ingest_json_bytes`) and enforces "never panic / never hang".

### Optional targets

Nova also has additional fuzz targets for deeper invariants / higher-level smoke tests:

```bash
cargo +nightly fuzz run parse_java -- -max_total_time=60 -max_len=262144
cargo +nightly fuzz run format_java -- -max_total_time=60 -max_len=262144
cargo +nightly fuzz run --features refactor refactor_smoke -- -max_total_time=60 -max_len=262144
```

- `format_java` asserts formatter idempotence (`format(format(x)) == format(x)`).
- `refactor_smoke` (requires the `refactor` Cargo feature) treats refactoring errors as expected and
  ignored; the target only enforces that Nova never panics or hangs while attempting a small set of
  best-effort refactorings.

### Remote protocol fuzzers (`nova-remote-*`)

Nova’s remote transport/protocol crates have their **own** `cargo-fuzz` harnesses:

- `crates/nova-remote-proto/fuzz/` (codec + framing):
  - `decode_framed_message`
  - `decode_v3_wire_frame`
  - `decode_v3_rpc_payload`
- `crates/nova-remote-rpc/fuzz/` (transport):
  - `v3_framed_transport`

Run these from the crate directory (not the repo root):

```bash
cd crates/nova-remote-proto
cargo +nightly fuzz list
cargo +nightly fuzz run decode_framed_message -- -max_total_time=60 -max_len=262144

cd ../nova-remote-rpc
cargo +nightly fuzz list
cargo +nightly fuzz run v3_framed_transport -- -max_total_time=60 -max_len=262144
```

### Debug Adapter Protocol fuzzers (`nova-dap`)

`crates/nova-dap` also has its own `cargo-fuzz` harness:

- `crates/nova-dap/fuzz/`:
  - `read_dap_message`

Run this from the crate directory (not the repo root):

```bash
cd crates/nova-dap
cargo +nightly fuzz list
cargo +nightly fuzz run read_dap_message -- -max_total_time=60 -max_len=262144
```

Seed corpora live under `fuzz/corpus/<target>/` (and under `crates/*/fuzz/corpus/<target>/` for
per-crate harnesses).

### JDWP fuzzers (`nova-jdwp`)

`crates/nova-jdwp` has a small per-crate fuzz harness focused on **JDWP packet framing** (length
prefix validation, bounded allocations, and hang resistance):

- `crates/nova-jdwp/fuzz/`:
  - `decode_packet_bytes`

Run this from the crate directory (not the repo root):

```bash
cd crates/nova-jdwp
cargo +nightly fuzz list
cargo +nightly fuzz run decode_packet_bytes -- -max_total_time=60 -max_len=262144
```

### Java seed corpus duplication (`fuzz_syntax_parse` / `fuzz_format`)

The Java seed corpora for the `fuzz_syntax_parse` and `fuzz_format` targets are intentionally
duplicated and are expected to stay **identical** (same `*.java` filenames and contents). This makes
it easy for new Java seeds to immediately benefit both fuzz targets.

To check that the two corpora haven't drifted:

```bash
bash scripts/check-fuzz-java-corpus-sync.sh
```

This check is also run as part of `./scripts/check-repo-invariants.sh` (and therefore in CI).

To re-sync after adding/removing/updating a Java seed, mirror the canonical corpus
(`fuzz/corpus/fuzz_syntax_parse/`) into `fuzz/corpus/fuzz_format/`:

```bash
bash scripts/sync-fuzz-java-corpus.sh
```

Some fuzz targets operate on the same kind of input (Java source text). To keep optional targets useful
out of the box, their checked-in seed corpora intentionally **reuse a small, curated subset** of the
primary Java corpora:

- `parse_java` ↔ `fuzz_syntax_parse`
- `format_java` ↔ `fuzz_format`
- `refactor_smoke` ↔ `fuzz_syntax_parse` (plus seeds that are representative for refactoring)

When adding new Java seeds, prefer updating the primary corpora first, then copy any especially useful
cases into the optional target corpus directories.

## Hangs, timeouts, and input size caps

Each fuzz target:

- caps the input to **256KiB** (to avoid OOM and pathological worst-case behavior)
- enforces a per-input wall-clock timeout and treats timeouts as fuzz failures (hangs)

## Crash artifacts and minimization

When a failure is found, libFuzzer writes the triggering input to:

`fuzz/artifacts/<target>/` (relative to the harness root):

- main harness: `./fuzz/artifacts/<target>/`
- per-crate harnesses: `./crates/<crate>/fuzz/artifacts/<target>/`

### Reproducing a failure

```bash
# Main harness (run from the repo root)
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<artifact>

# Per-crate harness (run from that crate directory)
(cd crates/nova-remote-proto && cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<artifact>)
```

### Minimizing a crash input

```bash
# Main harness (run from the repo root)
cargo +nightly fuzz tmin <target> fuzz/artifacts/<target>/<artifact>

# Per-crate harness (run from that crate directory)
(cd crates/nova-remote-proto && cargo +nightly fuzz tmin <target> fuzz/artifacts/<target>/<artifact>)
```

### Minimizing a corpus (optional)

If you have a large local corpus under `fuzz/corpus/<target>/` (or a per-crate `fuzz/corpus/<target>/`),
you can shrink it by running from that harness root:

```bash
cargo +nightly fuzz cmin <target>
```

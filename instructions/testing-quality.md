# Testing & Quality Workstream

> **⚠️ MANDATORY: Read and follow [AGENTS.md](../AGENTS.md) completely before proceeding.**
> **All rules in AGENTS.md apply at all times. This file adds workstream-specific guidance.**

---

## Scope

This workstream owns testing infrastructure, quality assurance, and fuzzing:

| Crate/Directory | Purpose |
|-----------------|---------|
| `nova-testing` | Test harnesses, test runners |
| `nova-test-utils` | Test utilities shared across crates |
| `fuzz/` | Fuzz testing targets |

---

## Key Documents

**Required reading:**
- [14 - Testing Strategy](../docs/14-testing-strategy.md) - Testing philosophy
- [14 - Testing Infrastructure](../docs/14-testing-infrastructure.md) - How to run tests
- [docs/fuzzing.md](../docs/fuzzing.md) - Fuzzing setup

---

## Testing Philosophy

```
┌─────────────────────────────────────────────────────────────────┐
│                    Testing Pyramid                               │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│                        ┌───────┐                                 │
│                        │  E2E  │  ← Few, slow, high confidence  │
│                       ┌┴───────┴┐                                │
│                       │ Integration │  ← Moderate                │
│                      ┌┴───────────┴┐                             │
│                      │    Unit      │  ← Many, fast, focused    │
│                     ┌┴─────────────┴┐                            │
│                     │   Property     │  ← Fuzzing, proptests    │
│                    └─────────────────┘                           │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

**Goals:**
1. **Correctness**: Match Java Language Specification
2. **Robustness**: Handle malformed input gracefully
3. **Performance**: Catch regressions early
4. **Coverage**: Test edge cases systematically

---

## Test Types

### Unit Tests

In-module tests for individual functions:

```rust
// crates/nova-types/src/subtype.rs

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn primitive_widening() {
        assert!(is_subtype(&Type::Int, &Type::Long));
        assert!(!is_subtype(&Type::Long, &Type::Int));
    }
}
```

### Integration Tests

Cross-crate tests in `tests/` directory:

```rust
// crates/nova-types/tests/suite/jls_inference.rs (run via crates/nova-types/tests/javac_differential.rs)

#[test]
fn diamond_inference() {
    let code = r#"
        import java.util.*;
        class Test {
            List<String> list = new ArrayList<>();
        }
    "#;
    
    let workspace = TestWorkspace::with_file("Test.java", code);
    let ty = workspace.type_of("Test", "list");
    
    assert_eq!(ty.to_string(), "List<String>");
}
```

### Snapshot Tests

Compare output against expected snapshots:

```rust
#[test]
fn parse_method_declaration() {
    let code = "void foo(int x, String y) { }";
    let tree = parse(code);
    
    // Compares against tests/snapshots/parse_method_declaration.snap
    insta::assert_snapshot!(format_tree(&tree));
}
```

**Update snapshots:**
```bash
# Nova has two snapshot systems:
# - File-based golden tests (parser snapshots, refactor fixtures): set `BLESS=1`
# - `insta` snapshots (formatter): set `INSTA_UPDATE=always`
#
# Parser golden corpus (`nova-syntax`): writes `crates/nova-syntax/testdata/**/*.tree` + `.errors`
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness suite::golden_corpus
#
# Refactor before/after fixtures (`nova-refactor`): writes `crates/nova-refactor/tests/fixtures/**/after/**`
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-refactor --test refactorings move_static_method_updates_call_sites
#
# Formatter snapshots (`nova-format`): writes `crates/nova-format/tests/snapshots/*.snap`
INSTA_UPDATE=always bash scripts/cargo_agent.sh test --locked -p nova-format --test harness suite::format_fixtures
INSTA_UPDATE=always bash scripts/cargo_agent.sh test --locked -p nova-format --test harness suite::format_snapshots
```

For the canonical “where do fixtures live / how do I bless them” workflow, see
[14 - Testing Infrastructure](../docs/14-testing-infrastructure.md) (and in agent runs, prefer
`bash scripts/cargo_agent.sh ...` over raw `cargo ...`).

### JLS Compliance Tests

Tests derived from Java Language Specification:

```
testdata/jls/
├── ch05_conversions/
│   ├── widening_primitive.java
│   ├── narrowing_primitive.java
│   └── boxing.java
├── ch06_names/
│   └── ...
└── ch15_expressions/
    └── ...
```

### Property Tests

Random input testing with proptest:

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn parse_never_panics(input in ".*") {
        let _ = parse(&input);  // Should never panic
    }
    
    #[test]
    fn type_transitivity(a: Type, b: Type, c: Type) {
        if is_subtype(&a, &b) && is_subtype(&b, &c) {
            prop_assert!(is_subtype(&a, &c));
        }
    }
}
```

---

## Fuzzing

For the authoritative fuzzing guide (target list, timeouts, artifacts, and minimization), see
[`docs/fuzzing.md`](../docs/fuzzing.md).

### Setup

```bash
# Install a nightly toolchain with LLVM tools (required by `cargo-fuzz`).
rustup toolchain install nightly --component llvm-tools-preview --component rust-src

# Install cargo-fuzz
# Recommended (fast): install the prebuilt cargo-fuzz binary via cargo-binstall.
bash scripts/cargo_agent.sh install cargo-binstall --locked
bash scripts/cargo_agent.sh +nightly binstall cargo-fuzz --version 0.13.1 --no-confirm --locked --disable-strategies compile --disable-telemetry

# List available fuzz targets in the main harness (`./fuzz/`).
bash scripts/cargo_agent.sh +nightly fuzz list

# Run a fuzz target (from the repo root).
bash scripts/cargo_agent.sh +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60 -max_len=262144

# Formatter / edit generation.
bash scripts/cargo_agent.sh +nightly fuzz run fuzz_format -- -max_total_time=60 -max_len=262144

# Parse JVM classfiles.
bash scripts/cargo_agent.sh +nightly fuzz run fuzz_classfile -- -max_total_time=60 -max_len=262144

# Parse JUnit XML reports.
bash scripts/cargo_agent.sh +nightly fuzz run fuzz_junit_report -- -max_total_time=60 -max_len=262144

# Read archives (ZIP/JAR/JMOD + directory mode).
bash scripts/cargo_agent.sh +nightly fuzz run fuzz_archive_read -- -max_total_time=60 -max_len=262144

# Optional targets
bash scripts/cargo_agent.sh +nightly fuzz run parse_java -- -max_total_time=60 -max_len=262144
bash scripts/cargo_agent.sh +nightly fuzz run format_java -- -max_total_time=60 -max_len=262144
bash scripts/cargo_agent.sh +nightly fuzz run --features refactor refactor_smoke -- -max_total_time=60 -max_len=262144

# IDE completions robustness (never panic on malformed Java + arbitrary cursor positions).
bash scripts/cargo_agent.sh +nightly fuzz run fuzz_completion -- -max_total_time=60 -max_len=262144

# Incremental parsing invariants (`nova_syntax::reparse_java`).
bash scripts/cargo_agent.sh +nightly fuzz run fuzz_reparse_java -- -max_total_time=60 -max_len=262144

# Workstation equivalent (no agent wrapper):
cargo +nightly fuzz run fuzz_reparse_java -- -max_total_time=60 -max_len=262144
```

### Fuzz Targets

```
fuzz/
├── Cargo.toml
├── corpus/           # Seed inputs
│   ├── fuzz_syntax_parse/
│   ├── fuzz_syntax_literals/
│   ├── fuzz_reparse_java/
│   ├── fuzz_reparse_java_sequence/
│   ├── fuzz_format/
│   ├── fuzz_range_format/
│   ├── fuzz_on_type_format/
│   ├── fuzz_classfile/
│   ├── fuzz_decompile_classfile/
│   ├── fuzz_junit_report/
│   ├── fuzz_completion/
│   ├── fuzz_yaml_parse/
│   ├── fuzz_properties_parse/
│   ├── fuzz_config_metadata/
│   ├── fuzz_archive_read/
│   ├── parse_java/
│   ├── format_java/
│   └── refactor_smoke/
└── fuzz_targets/
    ├── fuzz_syntax_parse.rs
    ├── fuzz_syntax_literals.rs
    ├── fuzz_reparse_java.rs          # incremental parsing / `nova_syntax::reparse_java` invariants
    ├── fuzz_reparse_java_sequence.rs
    ├── fuzz_format.rs
    ├── fuzz_range_format.rs
    ├── fuzz_on_type_format.rs
    ├── fuzz_classfile.rs
    ├── fuzz_decompile_classfile.rs
    ├── fuzz_junit_report.rs
    ├── fuzz_yaml_parse.rs
    ├── fuzz_properties_parse.rs
    ├── fuzz_config_metadata.rs
    ├── fuzz_archive_read.rs
    ├── parse_java.rs
    ├── format_java.rs                # formatter idempotence
    ├── refactor_smoke.rs             # requires `--features refactor`
    └── utils.rs
```

Nova also has per-crate fuzz harnesses for protocol/transport surface areas:

- `crates/nova-remote-proto/fuzz/`:
  - `decode_framed_message`
  - `decode_v3_wire_frame`
  - `decode_v3_rpc_payload`
- `crates/nova-remote-rpc/fuzz/`:
  - `v3_framed_transport`
- `crates/nova-dap/fuzz/`:
  - `read_dap_message`
- `crates/nova-jdwp/fuzz/`:
  - `decode_packet_bytes`

Run these from the crate directory:

```bash
cd crates/nova-remote-proto
bash ../../scripts/cargo_agent.sh +nightly fuzz list
bash ../../scripts/cargo_agent.sh +nightly fuzz run decode_framed_message -- -max_total_time=60 -max_len=262144
bash ../../scripts/cargo_agent.sh +nightly fuzz run decode_v3_wire_frame -- -max_total_time=60 -max_len=262144
bash ../../scripts/cargo_agent.sh +nightly fuzz run decode_v3_rpc_payload -- -max_total_time=60 -max_len=262144

cd ../nova-remote-rpc
bash ../../scripts/cargo_agent.sh +nightly fuzz list
bash ../../scripts/cargo_agent.sh +nightly fuzz run v3_framed_transport -- -max_total_time=60 -max_len=262144

cd ../nova-dap
bash ../../scripts/cargo_agent.sh +nightly fuzz list
bash ../../scripts/cargo_agent.sh +nightly fuzz run read_dap_message -- -max_total_time=60 -max_len=262144

cd ../nova-jdwp
bash ../../scripts/cargo_agent.sh +nightly fuzz list
bash ../../scripts/cargo_agent.sh +nightly fuzz run decode_packet_bytes -- -max_total_time=60 -max_len=262144
```

### Writing Fuzz Targets

```rust
// fuzz/fuzz_targets/fuzz_syntax_parse.rs (simplified)

#![no_main]
use libfuzzer_sys::fuzz_target;

mod utils;

fuzz_target!(|data: &[u8]| {
    let Some(text) = utils::truncate_utf8(data) else {
        return;
    };

    // Should never panic / hang on malformed input.
    let _ = nova_syntax::parse(text);
    let _ = nova_syntax::parse_java(text);
});
```

### Crash Triage

When fuzzer finds a crash:

See [`docs/fuzzing.md`](../docs/fuzzing.md) for artifact paths and minimization commands.

1. **Reproduce**: `bash scripts/cargo_agent.sh +nightly fuzz run <target> fuzz/artifacts/<target>/<artifact>`
2. **Minimize**: `bash scripts/cargo_agent.sh +nightly fuzz tmin <target> fuzz/artifacts/<target>/<artifact>`
3. **Debug**: Add minimized input as test case
4. **Fix**: Fix the bug
5. **Verify**: Ensure crash no longer reproduces

---

## Performance Testing

### Benchmarks

```rust
// crates/nova-core/benches/critical_paths.rs

use criterion::{criterion_group, criterion_main, Criterion};

fn benchmark_completion(c: &mut Criterion) {
    let workspace = setup_workspace();
    
    c.bench_function("completion_member_access", |b| {
        b.iter(|| {
            workspace.completions_at(file, offset)
        })
    });
}

criterion_group!(benches, benchmark_completion);
criterion_main!(benches);
```

**Run benchmarks:**
```bash
bash scripts/cargo_agent.sh bench --locked -p nova-core
```

### Threshold Tests

Runtime thresholds in `perf/runtime-thresholds.toml`:

```toml
[completion]
p50_ms = 30
p95_ms = 50
p99_ms = 100

[diagnostics]
p50_ms = 50
p95_ms = 100
p99_ms = 200
```

---

## Cross-Platform Testing

### Gotchas

See [Testing Infrastructure](../docs/14-testing-infrastructure.md) for details:

1. **Path canonicalization**: macOS `/var` → `/private/var`
2. **Path separators**: Use `Path::join`, not hardcoded `/`
3. **Line endings**: Normalize or use `.lines()`
4. **Case sensitivity**: Don't rely on case-sensitive paths

### CI Matrix

Tests run on:
- Ubuntu Linux x64
- macOS (arm64)
- Windows x64

---

## Test Organization

### DO:

```
crates/nova-foo/
├── src/
│   └── lib.rs        # Unit tests in #[cfg(test)] modules
└── tests/
    └── integration_tests.rs  # ONE binary
```

### DON'T:

```
crates/nova-foo/
└── tests/
    ├── test_a.rs     # BAD: Each file = separate binary
    ├── test_b.rs     # BAD: Slow compilation
    └── test_c.rs     # BAD: Memory pressure
```

---

## Running Tests

```bash
# Single crate
bash scripts/cargo_agent.sh test --locked -p nova-core --lib

# Specific test
bash scripts/cargo_agent.sh test --locked -p nova-types --lib -- test_name

# With output
bash scripts/cargo_agent.sh test --locked -p nova-syntax --lib -- --nocapture

# Update snapshots
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness suite::golden_corpus
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-refactor --test refactorings move_static_method_updates_call_sites
INSTA_UPDATE=always bash scripts/cargo_agent.sh test --locked -p nova-format --test harness suite::format_fixtures
INSTA_UPDATE=always bash scripts/cargo_agent.sh test --locked -p nova-format --test harness suite::format_snapshots

# Update `nova-testing` schema fixtures
UPDATE_SCHEMA_FIXTURES=1 bash scripts/cargo_agent.sh test --locked -p nova-testing --test integration suite::schema_json
```

**NEVER run:**
```bash
cargo test                    # Unbounded
cargo test --all-targets      # Will OOM
```

---

## Code Coverage

Generate coverage reports:

```bash
# Install llvm-cov
bash scripts/cargo_agent.sh install cargo-llvm-cov --locked

# Generate report
bash scripts/cargo_agent.sh llvm-cov --locked -p nova-core --html
```

---

## Common Pitfalls

1. **Flaky tests** - Use deterministic seeds, mock time
2. **Test pollution** - Isolate state between tests
3. **Slow tests** - Profile and optimize, or mark `#[ignore]`
4. **Missing edge cases** - Use property testing
5. **Platform differences** - Test on all CI platforms

---

## Dependencies

**Upstream:** All crates (testing is cross-cutting)
**Downstream:** CI, release quality

---

## Responsibilities

- Maintain test infrastructure
- Define testing standards
- Run and triage fuzzing
- Monitor test coverage
- Investigate flaky tests
- Performance benchmarking

---

*Remember: Always follow [AGENTS.md](../AGENTS.md) rules. Use wrapper scripts. Scope your cargo commands.*

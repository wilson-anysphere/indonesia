# `nova-syntax`

This crate contains the Java lexer and parser used by Nova.

## Golden corpus tests

The Java parser is tested using a fixture-driven golden corpus under
`crates/nova-syntax/testdata/`:

- `testdata/parser/**/*.java` — inputs expected to parse without errors
  - `*.tree` contains a debug dump of the produced syntax tree
- `testdata/recovery/**/*.java` — inputs expected to produce parse errors but still recover
  - `*.tree` contains a debug dump of the recovered syntax tree
  - `*.errors` contains canonicalized parse errors (`line:col: message`)

The golden corpus test is the `#[test] fn golden_corpus()` test (defined in
`crates/nova-syntax/tests/suite/golden_corpus.rs`) and is compiled into the `harness`
integration test binary (`crates/nova-syntax/tests/harness.rs`).

There is **no** standalone integration test target named `golden_corpus`, so you must run it via
`--test harness` (optionally filtering by test name).

Run the full `nova-syntax` integration test suite (`harness`):

```bash
bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness
```

Run just the golden corpus test (test-name filter):

```bash
bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness suite::golden_corpus
```

Update (or create) the expected outputs:

```bash
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness suite::golden_corpus
```

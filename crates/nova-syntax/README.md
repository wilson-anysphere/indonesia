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

Run just the corpus test:

```bash
bash scripts/cargo_agent.sh test -p nova-syntax --test golden_corpus
```

Update (or create) the expected outputs:

```bash
BLESS=1 bash scripts/cargo_agent.sh test -p nova-syntax --test golden_corpus
```

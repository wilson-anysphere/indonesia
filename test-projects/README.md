# Test project fixtures
The directories under `test-projects/` are **local-only fixtures** used by
ignored integration tests.

Clone them with:
```bash
./scripts/clone-test-projects.sh
```

Run the ignored “real project” test suites with:
```bash
./scripts/run-real-project-tests.sh
```

For CI-like behavior (and to reduce peak memory), run with a single test thread:
```bash
RUST_TEST_THREADS=1 ./scripts/run-real-project-tests.sh
```

To focus on a subset of fixtures (matches `clone-test-projects.sh`):
```bash
./scripts/run-real-project-tests.sh --only guava,spring-petclinic

# or:
NOVA_TEST_PROJECTS=guava,spring-petclinic ./scripts/run-real-project-tests.sh

# or (alias):
NOVA_REAL_PROJECT=guava,spring-petclinic ./scripts/run-real-project-tests.sh
```

Best-effort helper to compile the fixtures with their build toolchain (sanity-check your local JDK/Maven):
```bash
./scripts/javac-validate.sh

# Or compile only some fixtures:
./scripts/javac-validate.sh --only guava
NOVA_TEST_PROJECTS=guava ./scripts/javac-validate.sh
NOVA_REAL_PROJECT=guava ./scripts/javac-validate.sh
```

To focus on a subset of fixtures:
```bash
./scripts/javac-validate.sh --only guava,spring-petclinic

# or:
NOVA_TEST_PROJECTS=guava,spring-petclinic ./scripts/javac-validate.sh
NOVA_REAL_PROJECT=guava,spring-petclinic ./scripts/javac-validate.sh
```

Pinned revisions are recorded in `pins.toml` (single source of truth).

To clone/update only a subset of fixtures:

```bash
./scripts/clone-test-projects.sh --only guava,spring-petclinic

# or:
NOVA_TEST_PROJECTS=guava,spring-petclinic ./scripts/clone-test-projects.sh
NOVA_REAL_PROJECT=guava,spring-petclinic ./scripts/clone-test-projects.sh
```

## CI
These fixtures back ignored integration tests and are intentionally excluded
from normal `cargo test` runs.

The `Real project fixtures` GitHub Actions workflow runs the real-project tests
on a schedule, can be triggered manually, and also runs on pushes that touch the
real-project harness/pins. For manual runs, you can optionally provide a
comma-separated `only` input (e.g. `guava,spring-petclinic`) to limit which
fixtures are exercised.

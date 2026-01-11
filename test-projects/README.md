# Test project fixtures
The directories under `test-projects/` are **local-only fixtures** used by
ignored integration tests.

Clone them with:
```bash
./scripts/clone-test-projects.sh
```

Pinned revisions are recorded in `pins.toml` (single source of truth).

To clone/update only a subset of fixtures:

```bash
./scripts/clone-test-projects.sh --only guava,spring-petclinic

# or:
NOVA_TEST_PROJECTS=guava,spring-petclinic ./scripts/clone-test-projects.sh
```

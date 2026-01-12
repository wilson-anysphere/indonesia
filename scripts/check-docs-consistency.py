#!/usr/bin/env python3

from __future__ import annotations

import re
import sys
import json
import subprocess
from collections import Counter
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def check_architecture_map() -> list[str]:
    doc_path = REPO_ROOT / "docs" / "architecture-map.md"

    try:
        raw = subprocess.check_output(
            ["cargo", "metadata", "--format-version=1", "--no-deps", "--locked"],
            cwd=REPO_ROOT,
        )
        metadata = json.loads(raw)
        crates = sorted(p["name"] for p in metadata.get("packages", []))
    except Exception as e:
        return [f"failed to enumerate workspace crates via cargo metadata: {e}"]

    doc = read_text(doc_path)

    heading_re = re.compile(r"^### `([^`]+)`\s*$", flags=re.MULTILINE)
    matches = list(heading_re.finditer(doc))
    doc_crates_list = [m.group(1) for m in matches]
    doc_crates = set(doc_crates_list)

    errors: list[str] = []

    duplicates = sorted([c for c, n in Counter(doc_crates_list).items() if n > 1])
    if duplicates:
        errors.append(
            "docs/architecture-map.md contains duplicate crate headings for: "
            + ", ".join(duplicates)
        )

    missing = [c for c in crates if c not in doc_crates]
    extra = [c for c in sorted(doc_crates) if c not in crates]

    if missing:
        errors.append(
            "docs/architecture-map.md is missing crate headings for: "
            + ", ".join(missing)
        )
    if extra:
        errors.append(
            "docs/architecture-map.md contains headings for crates that no longer exist: "
            + ", ".join(extra)
        )

    required_fields = [
        "- **Purpose:**",
        "- **Key entry points:**",
        "- **Maturity:**",
        "- **Known gaps vs intended docs:**",
    ]
    for idx, m in enumerate(matches):
        crate = m.group(1)
        start = m.end()
        end = matches[idx + 1].start() if idx + 1 < len(matches) else len(doc)
        section = doc[start:end]
        for field in required_fields:
            if field not in section:
                errors.append(
                    f"docs/architecture-map.md crate section `{crate}` is missing required field: {field}"
                )
                break

    if not duplicates and not missing and not extra and doc_crates_list != crates:
        for idx, (actual, expected) in enumerate(zip(doc_crates_list, crates), start=1):
            if actual != expected:
                errors.append(
                    "docs/architecture-map.md crate headings are not in alphabetical order: "
                    f"entry {idx} is `{actual}` but expected `{expected}`"
                )
                break
    return errors


def extract_rust_methods(path: Path) -> set[str]:
    # Only consider string constants, not serde renames or other embedded strings.
    text = read_text(path)
    return set(
        re.findall(
            r'^\s*pub const [A-Z0-9_]+:\s*&str\s*=\s*"(nova/[^"]+)";\s*$',
            text,
            flags=re.MULTILINE,
        )
    )


def extract_vscode_methods() -> set[str]:
    # Best-effort: scan for `nova/*` string literals in the VS Code extension sources.
    # We exclude a small allowlist of known non-method strings used for error matching.
    ignore = {
        "nova/bugreport",  # substring match for the safe-mode error message
        "nova/refactor/preview",  # response `type` tag for safe delete previews
    }

    methods: set[str] = set()
    vscode_src = REPO_ROOT / "editors" / "vscode" / "src"
    for path in vscode_src.rglob("*.ts"):
        # Don't treat test-only strings as protocol methods (these often contain negative/placeholder cases).
        if (
            "__tests__" in path.parts
            or path.name.endswith(".test.ts")
            or path.name.endswith(".node-test.ts")
        ):
            continue
        text = read_text(path)
        # Only match plausible method names. This avoids false positives like human-readable
        # error messages that begin with `nova/...`.
        for m in re.findall(r"""['"](nova/[A-Za-z0-9_./-]+)['"]""", text):
            if m not in ignore:
                methods.add(m)
    return methods


def check_protocol_extensions() -> list[str]:
    doc_path = REPO_ROOT / "docs" / "protocol-extensions.md"
    doc = read_text(doc_path)
    heading_re = re.compile(r"^### `([^`]+)`", flags=re.MULTILINE)
    matches = [m for m in heading_re.finditer(doc) if m.group(1).startswith("nova/")]
    doc_methods_list = [m.group(1) for m in matches]
    doc_methods = {
        m
        for m in doc_methods_list
    }

    # Collect all `nova/*` method constants exposed by the `nova-lsp` crate.
    # We scan the whole crate to avoid drifting as files are refactored.
    rust_methods: set[str] = set()
    lsp_src = REPO_ROOT / "crates" / "nova-lsp" / "src"
    for path in lsp_src.rglob("*.rs"):
        rust_methods |= extract_rust_methods(path)

    vscode_methods = extract_vscode_methods()
    needed = rust_methods | vscode_methods

    errors: list[str] = []

    duplicates = sorted([m for m, n in Counter(doc_methods_list).items() if n > 1])
    if duplicates:
        errors.append(
            "docs/protocol-extensions.md contains duplicate method headings for: "
            + ", ".join(duplicates)
        )

    missing = sorted(m for m in needed if m not in doc_methods)
    if missing:
        errors.append(
            "docs/protocol-extensions.md is missing method headings for: "
            + ", ".join(missing)
        )

    extra = sorted(m for m in doc_methods if m not in needed)
    if extra:
        errors.append(
            "docs/protocol-extensions.md contains method headings not referenced by nova-lsp or the VS Code client: "
            + ", ".join(extra)
        )

    required_fields = ["- **Kind:**", "- **Stability:**"]
    for idx, m in enumerate(matches):
        method = m.group(1)
        start = m.end()
        end = matches[idx + 1].start() if idx + 1 < len(matches) else len(doc)
        section = doc[start:end]
        for field in required_fields:
            if field not in section:
                errors.append(
                    f"docs/protocol-extensions.md method section `{method}` is missing required field: {field}"
                )
                break

    return errors


def check_perf_docs() -> list[str]:
    """Ensure perf-related docs stay in sync with `.github/workflows/perf.yml`.

    The perf workflow is the source of truth for which Criterion bench suites are gated in CI.
    We intentionally keep the implementation lightweight (regex scanning) to avoid adding
    new Python dependencies (e.g. YAML parsers) to CI.
    """

    workflow_path = REPO_ROOT / ".github" / "workflows" / "perf.yml"
    if not workflow_path.exists():
        return []

    workflow = read_text(workflow_path)
    # Match `cargo bench` lines that specify a package + bench name.
    bench_re = re.compile(
        r"^\s*cargo bench[^\n]*\s-p\s+([^\s]+)[^\n]*\s--bench\s+([^\s]+)",
        flags=re.MULTILINE,
    )
    suites = sorted(set(bench_re.findall(workflow)))
    if not suites:
        return [
            "expected .github/workflows/perf.yml to contain `cargo bench -p <crate> --bench <name>` invocations"
        ]

    expected_bench_paths = {f"crates/{crate}/benches/{bench}.rs" for crate, bench in suites}

    # Docs we expect to mention the CI-gated suite.
    # - Strategy doc should reference the concrete bench file paths for readers.
    # - Infrastructure doc should include both the file paths and local-equivalent commands.
    docs_require_paths = [
        REPO_ROOT / "docs" / "14-testing-strategy.md",
        REPO_ROOT / "docs" / "14-testing-infrastructure.md",
    ]
    # perf/README is the operational reference; it should include runnable commands.
    perf_readme = REPO_ROOT / "perf" / "README.md"

    errors: list[str] = []

    # Perf config files that are part of the workflow/tooling contract.
    threshold_paths = [
        "perf/thresholds.toml",
        "perf/runtime-thresholds.toml",
    ]
    for rel in threshold_paths:
        if not (REPO_ROOT / rel).exists():
            errors.append(f"expected {rel} to exist")

    for crate, bench in suites:
        bench_path = f"crates/{crate}/benches/{bench}.rs"
        bench_file = REPO_ROOT / bench_path
        if not bench_file.exists():
            errors.append(
                f".github/workflows/perf.yml references `cargo bench -p {crate} --bench {bench}` but {bench_path} does not exist"
            )

    bench_path_re = re.compile(r"crates/[A-Za-z0-9_-]+/benches/[A-Za-z0-9_-]+\.rs")
    bench_cmd_re = re.compile(
        r"(?:cargo bench|bash\s+(?:\./)?scripts/cargo_agent\.sh\s+bench)[^\n]*\s-p\s+([^\s]+)[^\n]*\s--bench\s+([^\s`]+)"
    )
    for doc_path in docs_require_paths:
        if not doc_path.exists():
            errors.append(f"expected {doc_path.relative_to(REPO_ROOT)} to exist")
            continue
        doc = read_text(doc_path)
        present = set(bench_path_re.findall(doc))
        missing = sorted(expected_bench_paths - present)
        extra = sorted(present - expected_bench_paths)
        if missing:
            errors.append(
                f"{doc_path.relative_to(REPO_ROOT)} is missing CI perf bench paths: "
                + ", ".join(missing)
            )
        if extra:
            errors.append(
                f"{doc_path.relative_to(REPO_ROOT)} mentions bench paths not gated by `.github/workflows/perf.yml`: "
                + ", ".join(extra)
            )

        present_cmds = set(bench_cmd_re.findall(doc))
        missing_cmds = sorted(set(suites) - present_cmds)
        extra_cmds = sorted(present_cmds - set(suites))
        if missing_cmds:
            errors.append(
                f"{doc_path.relative_to(REPO_ROOT)} is missing CI perf bench commands for: "
                + ", ".join(f"{crate}::{bench}" for crate, bench in missing_cmds)
            )
        if extra_cmds:
            errors.append(
                f"{doc_path.relative_to(REPO_ROOT)} includes bench commands not gated by `.github/workflows/perf.yml`: "
                + ", ".join(f"{crate}::{bench}" for crate, bench in extra_cmds)
            )
        for rel in threshold_paths:
            if rel not in doc:
                errors.append(
                    f"{doc_path.relative_to(REPO_ROOT)} does not mention {rel} (perf tooling config)"
                )

    if not perf_readme.exists():
        errors.append("expected perf/README.md to exist")
    else:
        perf_doc = read_text(perf_readme)
        readme_bench_re = re.compile(
            r"^\s*cargo bench[^\n]*\s-p\s+([^\s]+)[^\n]*\s--bench\s+([^\s]+)",
            flags=re.MULTILINE,
        )
        present_suites = set(readme_bench_re.findall(perf_doc))
        missing_suites = sorted(set(suites) - present_suites)
        extra_suites = sorted(present_suites - set(suites))
        if missing_suites:
            errors.append(
                "perf/README.md is missing CI bench commands for: "
                + ", ".join(f"{crate}::{bench}" for crate, bench in missing_suites)
            )
        if extra_suites:
            errors.append(
                "perf/README.md includes bench commands not gated by `.github/workflows/perf.yml`: "
                + ", ".join(f"{crate}::{bench}" for crate, bench in extra_suites)
            )
        for rel in threshold_paths:
            if rel not in perf_doc:
                errors.append(f"perf/README.md does not mention {rel} (perf tooling config)")

    return errors


def main() -> int:
    errors: list[str] = []
    errors.extend(check_architecture_map())
    errors.extend(check_protocol_extensions())
    errors.extend(check_perf_docs())

    if errors:
        for err in errors:
            print(f"error: {err}", file=sys.stderr)
        return 1

    print("docs consistency check: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

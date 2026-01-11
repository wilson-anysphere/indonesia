#!/usr/bin/env python3

from __future__ import annotations

import re
import sys
from collections import Counter
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def check_architecture_map() -> list[str]:
    crates_dir = REPO_ROOT / "crates"
    doc_path = REPO_ROOT / "docs" / "architecture-map.md"

    crates = sorted(p.name for p in crates_dir.iterdir() if p.is_dir())
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
        text = read_text(path)
        for m in re.findall(r"""['"](nova/[^'"]+)['"]""", text):
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


def main() -> int:
    errors: list[str] = []
    errors.extend(check_architecture_map())
    errors.extend(check_protocol_extensions())

    if errors:
        for err in errors:
            print(f"error: {err}", file=sys.stderr)
        return 1

    print("docs consistency check: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

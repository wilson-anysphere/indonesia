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

    doc_crates_list = re.findall(r"^### `([^`]+)`\s*$", doc, flags=re.MULTILINE)
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
    doc_methods_list = [
        m for m in re.findall(r"^### `([^`]+)`", doc, flags=re.MULTILINE) if m.startswith("nova/")
    ]
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

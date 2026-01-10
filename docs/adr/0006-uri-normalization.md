# ADR 0006: Path/URI normalization and virtual document schemes

## Context

Nova must refer to documents across multiple “storage backends”:

- normal on-disk source files,
- sources inside JAR/JMOD archives (e.g., `src.jar`, attached sources),
- decompiled virtual documents (generated on demand from `.class` files).

LSP uses URIs as stable identifiers across requests. If URI normalization is inconsistent, Nova will mis-key caches, duplicate documents, and break navigation.

## Decision

Use a canonical internal representation for “document identity” with explicit variants and a single normalization path at the protocol boundary.

- Internally, prefer a structured type (e.g. `nova-vfs::VfsPath` / `ArchivePath`) over raw strings.
- Externally (LSP/DAP), use URIs that round-trip cleanly and can be used as stable cache keys.

### Canonical schemes

1. **On-disk files**
   - LSP URI scheme: `file`
   - Canonical form: absolute path, normalized separators, percent-encoded as required by RFC 3986.

2. **Archive entries (JAR/JMOD)**
   - LSP URI scheme: `jar`
   - Canonical form:
     - `jar:///ABSOLUTE/PATH/TO/archive.jar!/path/inside/archive/Entry.java`
     - `jar:///ABSOLUTE/PATH/TO/archive.jmod!/path/inside/archive/Entry.class`
   - Rules:
     - archive path is absolute and normalized,
     - `!` separates archive path from entry path,
     - entry path always uses `/` and MUST NOT contain `..`.

3. **Decompiled virtual documents**
   - LSP URI scheme: `nova`
   - Canonical form:
     - `nova:///decompiled/<content-hash>/<binary-name>.java`
   - `<content-hash>` is a stable hash over the bytecode + decompiler version so URIs change when the rendered content changes.

### Normalization rules

- URI parsing/printing uses `lsp_types::Uri` / `url::Url` at the protocol boundary (not in low-level crates that intentionally avoid heavy dependencies).
- All normalization happens *once* at ingress:
  - incoming URIs are parsed → converted into the internal document identity type → used as map keys.
- For `file:` URIs:
  - prefer logical normalization (clean `.`/`..`) over filesystem canonicalization to avoid forcing symlink resolution and to handle non-existent-but-open documents.
- For non-file URIs:
  - treat the URI as an opaque identifier after parsing/validation; do not attempt to “canonicalize” via filesystem calls.

## Alternatives considered

### A. Use raw `Url` everywhere

Pros:
- simplest representation.

Cons:
- normalization rules become scattered and inconsistent,
- easy to accidentally treat semantically-equal URIs as distinct (percent encoding, path casing, etc.).

### B. Reuse Java’s `jar:file:///...!/` URI form

Pros:
- familiar to Java tooling.

Cons:
- nested URI forms are awkward to parse/print consistently in Rust,
- more opportunities for inconsistent escaping.

## Consequences

Positive:
- stable, explicit identifiers for all document types,
- consistent cache keys across LSP, VFS, and query layers,
- enables virtual documents without pretending they are real files.

Negative:
- editors/clients may not natively recognize non-`file` URIs; Nova must ensure “open/show document” flows work with clients that support custom schemes.

## Follow-ups

- Decide whether to support JDK `jrt:` URIs as a first-class variant (modules in the runtime image).
- Provide conversion helpers:
  - internal document identity type `<-> Url`
  - internal document identity type `<-> FileId`
- Add unit tests for normalization edge cases (percent encoding, Windows paths, `!` parsing, `..` rejection).

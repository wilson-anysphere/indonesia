# Helix setup (template)

This template configures Helix's built-in LSP client to launch `nova-lsp` over stdio for Java files.

## Prerequisites

- Helix
- `nova-lsp` available on your `$PATH`

## `languages.toml`

Add the following to `~/.config/helix/languages.toml` (create the file if it doesn’t exist):

```toml
[language-server.nova-lsp]
command = "nova-lsp"
args = ["--stdio"]

[[language]]
name = "java"
language-id = "java"
language-servers = ["nova-lsp"]
roots = [
  "pom.xml",
  "build.gradle",
  "build.gradle.kts",
  "settings.gradle",
  "settings.gradle.kts",
  "WORKSPACE",
  "WORKSPACE.bazel",
  "MODULE.bazel",
  ".git",
]
```

## Nova custom requests (optional)

Nova defines custom LSP methods under the `nova/*` namespace. For the stable spec, see
[`docs/protocol-extensions.md`](../../docs/protocol-extensions.md).

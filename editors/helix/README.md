# Helix setup (template)

This template configures Helix's built-in LSP client to launch Nova's LSP server over stdio for Java files.

## Prerequisites

- Helix
- `nova` available on your `$PATH` (recommended), or `nova-lsp` if you prefer to run the server binary directly.

## `languages.toml`

Add the following to `~/.config/helix/languages.toml` (create the file if it doesn’t exist):

```toml
[language-server.nova-lsp]
command = "nova"
args = ["lsp"]

[[language]]
name = "java"
language-id = "java"
language-servers = ["nova-lsp"]
roots = [
  "nova.toml",
  ".nova.toml",
  "nova.config.toml",
  ".nova/config.toml",
  ".nova",
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

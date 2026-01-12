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

## AI multi-token completions (server-side overrides)

Nova’s **multi-token completions** are computed asynchronously by the server and surfaced via
`nova/completion/more` (see [`docs/protocol-extensions.md`](../../docs/protocol-extensions.md)).

If you want to control or disable these completions without changing `nova.toml`, set the server
startup environment variable `NOVA_AI_COMPLETIONS_MAX_ITEMS` when launching Helix (or otherwise
ensure it is present in Helix’s environment). This is read at server startup, so you must restart
the language server for changes to take effect.

Example (disable multi-token completions entirely):

```bash
NOVA_AI_COMPLETIONS_MAX_ITEMS=0 helix
```

## Nova custom requests (optional)

Nova defines custom LSP methods under the `nova/*` namespace. For the stable spec, see
[`docs/protocol-extensions.md`](../../docs/protocol-extensions.md).

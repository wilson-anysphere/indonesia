# Sublime Text setup (template)

This template configures the [Sublime Text LSP package](https://packagecontrol.io/packages/LSP) to
launch Nova's LSP server over stdio for Java files.

## Prerequisites

- Sublime Text
- The `LSP` package (via Package Control)
- `nova` available on your `$PATH` (recommended), or `nova-lsp` if you prefer to run the server binary directly.

## Configure the LSP client

Create `Packages/User/LSP-nova-lsp.sublime-settings` (Preferences → Browse Packages…) with:

```json
{
  "clients": {
    "nova-lsp": {
      "enabled": true,
      "command": ["nova", "lsp"],
      "selector": "source.java"
    }
  }
}
```

Restart Sublime Text (or use `LSP: Restart Server`) and open a `.java` file.

## AI multi-token completions (server-side overrides)

Nova’s **multi-token completions** are computed asynchronously by the server and surfaced via
`nova/completion/more` (see [`docs/protocol-extensions.md`](../../docs/protocol-extensions.md)).

If you want to control or disable these completions without changing `nova.toml`, set the server
startup environment variable `NOVA_AI_COMPLETIONS_MAX_ITEMS` for the `nova-lsp` process.
This is read at server startup, so you must restart the server (or restart Sublime Text) for changes
to take effect.

For example, to disable multi-token completions entirely, ensure Sublime Text is launched with:

```bash
export NOVA_AI_COMPLETIONS_MAX_ITEMS=0
```

## Nova custom requests (optional)

Nova defines custom LSP methods under the `nova/*` namespace. For the stable spec, see
[`docs/protocol-extensions.md`](../../docs/protocol-extensions.md).

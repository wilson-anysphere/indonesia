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

## Nova custom requests (optional)

Nova defines custom LSP methods under the `nova/*` namespace. For the stable spec, see
[`docs/protocol-extensions.md`](../../docs/protocol-extensions.md).

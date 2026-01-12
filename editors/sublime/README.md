# Sublime Text setup (template)

This template configures the [Sublime Text LSP package](https://packagecontrol.io/packages/LSP) to
launch `nova-lsp` over stdio for Java files.

## Prerequisites

- Sublime Text
- The `LSP` package (via Package Control)
- `nova-lsp` available on your `$PATH`

## Configure the LSP client

Create `Packages/User/LSP-nova-lsp.sublime-settings` (Preferences → Browse Packages…) with:

```json
{
  "clients": {
    "nova-lsp": {
      "enabled": true,
      "command": ["nova-lsp", "--stdio"],
      "selector": "source.java"
    }
  }
}
```

Restart Sublime Text (or use `LSP: Restart Server`) and open a `.java` file.

## Nova custom requests (optional)

Nova defines custom LSP methods under the `nova/*` namespace. For the stable spec, see
[`docs/protocol-extensions.md`](../../docs/protocol-extensions.md).

# Neovim setup (template)

This template configures Neovim's built-in LSP client to launch `nova-lsp` over stdio for Java buffers.

## Prerequisites

- Neovim 0.8+ (0.10+ recommended)
- [`neovim/nvim-lspconfig`](https://github.com/neovim/nvim-lspconfig)
- `nova-lsp` available on your `$PATH`

## `nvim-lspconfig` configuration

```lua
local lspconfig = require('lspconfig')
local configs = require('lspconfig.configs')
local util = require('lspconfig.util')

if not configs.nova then
  configs.nova = {
    default_config = {
      cmd = { 'nova-lsp', '--stdio' },
      filetypes = { 'java' },
      root_dir = util.root_pattern('pom.xml', 'build.gradle', 'settings.gradle', '.git'),
    },
  }
end

lspconfig.nova.setup({})
```

## Calling Nova custom requests (optional)

Nova defines custom LSP methods under the `nova/*` namespace. For the stable spec, see
[`docs/protocol-extensions.md`](../../docs/protocol-extensions.md).

### Organize imports

`nova-lsp` does **not** currently implement the custom request `nova/java/organizeImports`; prefer
the standard LSP code action kind `source.organizeImports`:

```lua
vim.api.nvim_create_user_command('NovaOrganizeImports', function()
  vim.lsp.buf.code_action({
    context = { only = { 'source.organizeImports' } },
  })
end, {})
```

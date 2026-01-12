# Neovim setup (template)

This template configures Neovim's built-in LSP client to launch `nova-lsp` over stdio for Java buffers.

The repo includes a copy/paste-ready config file at [`editors/neovim/init.lua`](./init.lua).

## Prerequisites

- Neovim 0.8+ (0.10+ recommended)
- [`neovim/nvim-lspconfig`](https://github.com/neovim/nvim-lspconfig)
- `nova-lsp` available on your `$PATH`

## Quick start

1. Install [`neovim/nvim-lspconfig`](https://github.com/neovim/nvim-lspconfig).
2. Copy [`editors/neovim/init.lua`](./init.lua) to your Neovim config directory:
   - Linux/macOS: `~/.config/nvim/init.lua`
   - Windows: `%LOCALAPPDATA%\nvim\init.lua`
3. Ensure `nova-lsp` is available on your `$PATH`.

The template includes:

- `nova-lsp --stdio` for `java` buffers via `nvim-lspconfig`
- Root detection via `pom.xml`, `build.gradle(.kts)`, `settings.gradle(.kts)`, `.git`
- `:NovaOrganizeImports` + `<leader>oi` helper mapping (standard LSP `source.organizeImports`)

## `nvim-lspconfig` configuration (inline snippet)

```lua
local lspconfig = require('lspconfig')
local configs = require('lspconfig.configs')
local util = require('lspconfig.util')

if not configs.nova then
  configs.nova = {
    default_config = {
      cmd = { 'nova-lsp', '--stdio' },
      filetypes = { 'java' },
      root_dir = util.root_pattern('pom.xml', 'build.gradle', 'build.gradle.kts', 'settings.gradle', 'settings.gradle.kts', '.git'),
    },
  }
end

lspconfig.nova.setup({})
```

## Calling Nova custom requests (optional)

Nova defines custom LSP methods under the `nova/*` namespace. For the stable spec, see
[`docs/protocol-extensions.md`](../../docs/protocol-extensions.md).

### Organize imports

`nova-lsp` supports the standard LSP code action kind `source.organizeImports` (recommended for
portability). Nova also exposes a custom request `nova/java/organizeImports` for some clients; see
[`docs/protocol-extensions.md`](../../docs/protocol-extensions.md) for details.

```lua
vim.api.nvim_create_user_command('NovaOrganizeImports', function()
  vim.lsp.buf.code_action({
    context = { only = { 'source.organizeImports' } },
  })
end, {})
```

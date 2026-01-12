# Neovim setup (template)

This template configures Neovim's built-in LSP client to launch `nova-lsp` over stdio for Java buffers.

The repo includes a copy/paste-ready config file at [`editors/neovim/init.lua`](./init.lua).

## Prerequisites

- Neovim 0.8+ (0.10+ recommended)
- [`neovim/nvim-lspconfig`](https://github.com/neovim/nvim-lspconfig)
- `nova-lsp` available on your `$PATH`

## `nvim-lspconfig` configuration

```lua
local lspconfig = require('lspconfig')
local configs = require('lspconfig.configs')
local util = require('lspconfig.util')

if not configs.nova_lsp then
  configs.nova_lsp = {
    default_config = {
      cmd = { 'nova-lsp', '--stdio' },
      filetypes = { 'java' },
      root_dir = function(fname)
        return util.root_pattern(
          'pom.xml',
          'build.gradle',
          'build.gradle.kts',
          'settings.gradle',
          'settings.gradle.kts',
          'MODULE.bazel',
          'WORKSPACE',
          'WORKSPACE.bazel',
          '.git',
          '.nova'
        )(fname) or util.path.dirname(fname)
      end,
      single_file_support = true,
    },
  }
end

lspconfig.nova_lsp.setup({})
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

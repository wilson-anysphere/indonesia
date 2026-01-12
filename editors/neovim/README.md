# Neovim setup (template)

This template configures Neovim's built-in LSP client to launch Nova's LSP server over stdio for Java buffers.

The repo includes a copy/paste-ready config file at [`editors/neovim/init.lua`](./init.lua).

## Prerequisites

- Neovim 0.8+ (0.10+ recommended)
- [`neovim/nvim-lspconfig`](https://github.com/neovim/nvim-lspconfig)
- `nova` available on your `$PATH` (recommended), or `nova-lsp` if you prefer to run the server binary directly.

## Quick start

1. Install [`neovim/nvim-lspconfig`](https://github.com/neovim/nvim-lspconfig).
2. Copy [`editors/neovim/init.lua`](./init.lua) to your Neovim config directory:
   - Linux/macOS: `~/.config/nvim/init.lua`
   - Windows: `%LOCALAPPDATA%\nvim\init.lua`
3. Ensure `nova` is available on your `$PATH`.

The template includes:

- `nova lsp` for `java` buffers via `nvim-lspconfig` (stdio)
- Root detection via Nova/Maven/Gradle/Bazel markers: `nova.toml`, `pom.xml`, `build.gradle(.kts)`, `settings.gradle(.kts)`, `WORKSPACE(.bazel)`, `MODULE.bazel`, `.git`, `.nova`
- `:NovaOrganizeImports` + `<leader>oi` helper mapping (standard LSP `source.organizeImports`)

## `nvim-lspconfig` configuration (inline snippet)

```lua
-- Minimal Neovim LSP setup for Nova (stdio) for Java.
--
-- This file is meant to be copied into your own Neovim config. It assumes you
-- have `neovim/nvim-lspconfig` installed and `nova` available on $PATH.
--
-- Copy to:
--   - Linux/macOS: ~/.config/nvim/init.lua
--   - Windows:    %LOCALAPPDATA%\\nvim\\init.lua

local ok, lspconfig = pcall(require, "lspconfig")
if not ok then
  vim.api.nvim_err_writeln("nova-lsp: missing dependency 'nvim-lspconfig'")
  return
end

local configs = require("lspconfig.configs")
local util = require("lspconfig.util")

-- `nvim-lspconfig` doesn't ship a built-in `nova-lsp` config, so define one if it
-- doesn't already exist.
if not configs.nova_lsp then
  configs.nova_lsp = {
    default_config = {
      cmd = { "nova", "lsp" },
      filetypes = { "java" },
      root_dir = util.root_pattern(
        -- Nova config.
        "nova.toml",
        ".nova.toml",
        "nova.config.toml",
        ".nova/config.toml",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
        -- Bazel workspace markers.
        "WORKSPACE",
        "WORKSPACE.bazel",
        "MODULE.bazel",
        ".git",
        ".nova"
      ),
    },
  }
end

local function organize_imports()
  vim.lsp.buf.code_action({
    context = { only = { "source.organizeImports" } },
  })
end

local function on_attach(_, bufnr)
  vim.api.nvim_buf_create_user_command(bufnr, "NovaOrganizeImports", organize_imports, {
    desc = "Organize imports (source.organizeImports)",
  })

  -- <leader>oi = "organize imports"
  vim.keymap.set("n", "<leader>oi", "<cmd>NovaOrganizeImports<CR>", {
    buffer = bufnr,
    desc = "Organize imports",
  })
end

lspconfig.nova_lsp.setup({
  on_attach = on_attach,
})
```

## Calling Nova custom requests (optional)

Nova defines custom LSP methods under the `nova/*` namespace. For the stable spec, see
[`docs/protocol-extensions.md`](../../docs/protocol-extensions.md).

## AI multi-token completions (server-side overrides)

Nova’s **multi-token completions** are computed asynchronously by the server and surfaced via
`nova/completion/more` (see [`docs/protocol-extensions.md`](../../docs/protocol-extensions.md)).

If you want to control or disable these completions without changing `nova.toml`, you can set
server startup environment variables:

- `NOVA_AI_COMPLETIONS_MAX_ITEMS=0` disables multi-token completions entirely.
- `NOVA_AI_COMPLETIONS_MAX_ITEMS=<n>` caps the number of AI completion items (values are clamped by
  the server; restart required).

With `nvim-lspconfig`, you can set these via `cmd_env`:

```lua
lspconfig.nova_lsp.setup({
  -- ...
  cmd_env = {
    NOVA_AI_COMPLETIONS_MAX_ITEMS = "0",
  },
})
```

### Organize imports

Nova's LSP server (`nova-lsp`, launched via `nova lsp`) supports the standard LSP code action kind `source.organizeImports` (recommended for
portability). Nova also exposes a custom request `nova/java/organizeImports` for some clients; see
[`docs/protocol-extensions.md`](../../docs/protocol-extensions.md) for details.

```lua
vim.api.nvim_create_user_command('NovaOrganizeImports', function()
  vim.lsp.buf.code_action({
    context = { only = { 'source.organizeImports' } },
  })
end, {})
```

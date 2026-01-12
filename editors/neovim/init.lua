-- Minimal Neovim LSP setup for `nova-lsp` (stdio) for Java.
--
-- This file is meant to be copied into your own Neovim config. It assumes you
-- have `neovim/nvim-lspconfig` installed and `nova-lsp` available on $PATH.
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
if not configs.nova then
  configs.nova = {
    default_config = {
      cmd = { "nova-lsp", "--stdio" },
      filetypes = { "java" },
      root_dir = util.root_pattern(
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
        ".git"
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

lspconfig.nova.setup({
  on_attach = on_attach,
})
